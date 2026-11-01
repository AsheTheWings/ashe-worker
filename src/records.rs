//! Upload of finished days to the server, and local retention of uploaded days.
//!
//! A day is finished once its daily report grace period has passed, no block
//! of it is pending, and its daily report is current when reports are enabled.
//! Every file of a finished day except captures is sent once with
//! `PUT <server>/v1/records/<day>/<path>`; the server stores each path once
//! and acknowledges an identical repeat. The upload ledger records each
//! acknowledged digest, and a day leaves this machine only when every one of
//! its files is acknowledged with its current content.

use crate::config::AppConfig;
use crate::{daily_report, logger};
use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{Days, Local, NaiveDate};
use opentelemetry::KeyValue;
use opentelemetry::trace::{Span, Status, Tracer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

pub const RECORD_FORMAT: &str = "1";
const LEDGER_FILE: &str = "upload-ledger.json";
/// Top-level day directories that hold screen captures, which never leave this machine.
const CAPTURE_DIRECTORIES: [&str; 2] = ["frames", "keyframes"];
const MAX_SEGMENTS: usize = 8;
const MAX_SEGMENT_BYTES: usize = 128;

pub struct RecordsHandle {
    stop: Option<Sender<()>>,
}

impl RecordsHandle {
    pub fn spawn(config: AppConfig) -> Self {
        let (stop, receiver) = mpsc::channel();
        std::thread::spawn(move || run(config, receiver));
        Self { stop: Some(stop) }
    }
}

impl Drop for RecordsHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn run(config: AppConfig, stop: Receiver<()>) {
    let interval = Duration::from_secs(config.upload_scan_minutes.saturating_mul(60));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build();
    let (Ok(runtime), Ok(client)) = (runtime, client) else {
        logger::info("Record upload could not start: HTTP client failed");
        return;
    };
    loop {
        scan(&config, &runtime, &client);
        match stop.recv_timeout(interval) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn scan(config: &AppConfig, runtime: &tokio::runtime::Runtime, client: &reqwest::Client) {
    let tracer = opentelemetry::global::tracer("ashe-worker");
    let mut span = tracer.start("ashe.worker.records.scan");
    let result = runtime.block_on(scan_once(config, client, Local::now().date_naive()));
    match &result {
        Ok(outcome) => {
            span.set_attribute(KeyValue::new("ashe.worker.outcome", "success"));
            span.set_attribute(KeyValue::new(
                "ashe.worker.records.uploaded",
                outcome.uploaded as i64,
            ));
            span.set_attribute(KeyValue::new(
                "ashe.worker.records.removed_days",
                outcome.removed_days as i64,
            ));
            span.set_status(Status::Ok);
            if outcome.uploaded > 0 || outcome.removed_days > 0 {
                logger::info(format!(
                    "Record upload scan acknowledged files={} removed_days={}",
                    outcome.uploaded, outcome.removed_days
                ));
            }
        }
        Err(error) => {
            let class = error_class(error);
            span.set_attribute(KeyValue::new("ashe.worker.outcome", "error"));
            span.set_attribute(KeyValue::new("error.type", class));
            span.set_status(Status::error(class));
            logger::info(format!("Record upload scan failed: {class}"));
        }
    }
    span.end();
}

/// Failure classes of record upload, bounded for telemetry.
#[derive(Debug)]
pub enum UploadFailure {
    /// The server could not be reached or answered with a server error.
    Unavailable,
    /// The server refused the token.
    Unauthorized,
    /// The server already holds different content at a path.
    Conflict,
    /// The server refused the request as malformed.
    Rejected,
}

impl std::fmt::Display for UploadFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "the server is unavailable",
            Self::Unauthorized => "the server refused the token",
            Self::Conflict => "the server holds different content at a path",
            Self::Rejected => "the server rejected a record",
        })
    }
}

impl std::error::Error for UploadFailure {}

fn error_class(error: &anyhow::Error) -> &'static str {
    match error.downcast_ref::<UploadFailure>() {
        Some(UploadFailure::Unavailable) => "dependency",
        Some(UploadFailure::Unauthorized) => "unauthorized",
        Some(UploadFailure::Conflict) => "conflict",
        Some(UploadFailure::Rejected) => "rejected",
        None => "operational",
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    pub uploaded: usize,
    pub removed_days: usize,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct Ledger {
    /// Acknowledged SHA-256 digests by day and relative path.
    #[serde(default)]
    days: BTreeMap<String, BTreeMap<String, String>>,
}

/// One record of a day: its relative path, content, and SHA-256 digest.
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    pub path: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// Upload every unacknowledged record of each finished day, then remove
/// uploaded days older than the retention window.
pub async fn scan_once(
    config: &AppConfig,
    client: &reqwest::Client,
    today: NaiveDate,
) -> Result<ScanOutcome> {
    let root = &config.activity_artifacts_dir;
    if !root.is_dir() {
        return Ok(ScanOutcome::default());
    }
    let mut ledger = read_ledger(root)?;
    let mut outcome = ScanOutcome::default();
    let mut first_error = None;
    for (day, directory) in day_directories(root)? {
        if day >= today {
            continue;
        }
        let day_text = day.format("%Y-%m-%d").to_string();
        if !day_finished(config, &day_text) {
            continue;
        }
        let records = {
            let _guard = crate::artifact_store::lock(root)?;
            collect_day(&directory)
        };
        let records = match records {
            Ok(records) => records,
            Err(error) => {
                logger::info(format!(
                    "Record upload deferred for {day_text}: day failed validation"
                ));
                first_error.get_or_insert(error);
                continue;
            }
        };
        let mut complete = true;
        for record in &records {
            let acknowledged = ledger
                .days
                .get(&day_text)
                .and_then(|files| files.get(&record.path));
            if acknowledged == Some(&record.sha256) {
                continue;
            }
            if let Err(error) = put_record(client, config, &day_text, record).await {
                complete = false;
                let stop = matches!(
                    error.downcast_ref::<UploadFailure>(),
                    Some(UploadFailure::Unavailable | UploadFailure::Unauthorized)
                );
                first_error.get_or_insert(error);
                if stop {
                    return Err(first_error.expect("error recorded"));
                }
                break;
            }
            ledger
                .days
                .entry(day_text.clone())
                .or_default()
                .insert(record.path.clone(), record.sha256.clone());
            write_ledger(root, &ledger)?;
            outcome.uploaded += 1;
        }
        if complete && !records.is_empty() && expired(day, today, config.local_retention_days) {
            let removed = {
                let _guard = crate::artifact_store::lock(root)?;
                let unchanged = collect_day(&directory).is_ok_and(|current| current == records);
                if unchanged {
                    fs::remove_dir_all(&directory)
                        .with_context(|| format!("failed to remove uploaded day {day_text}"))?;
                }
                unchanged
            };
            if removed {
                ledger.days.remove(&day_text);
                write_ledger(root, &ledger)?;
                outcome.removed_days += 1;
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(outcome),
    }
}

fn day_finished(config: &AppConfig, day: &str) -> bool {
    daily_report::grace_period_elapsed(day, config.daily_report_grace_minutes)
        && !daily_report::has_pending_blocks(&config.activity_artifacts_dir, day)
        && (!config.daily_report_enabled || daily_report::report_is_current(config, day))
}

fn expired(day: NaiveDate, today: NaiveDate, retention_days: u64) -> bool {
    today
        .checked_sub_days(Days::new(retention_days))
        .is_some_and(|oldest_kept| day < oldest_kept)
}

async fn put_record(
    client: &reqwest::Client,
    config: &AppConfig,
    day: &str,
    record: &Record,
) -> Result<()> {
    let url = format!("{}/v1/records/{day}/{}", config.server_url, record.path);
    let response = client
        .put(url)
        .bearer_auth(&config.server_token)
        .header("content-type", "application/octet-stream")
        .header("x-ashe-record-format", RECORD_FORMAT)
        .header("x-ashe-record-sha256", &record.sha256)
        .body(record.bytes.clone())
        .send()
        .await
        .map_err(|_| UploadFailure::Unavailable)?;
    match response.status().as_u16() {
        200 | 201 => Ok(()),
        401 | 403 => Err(UploadFailure::Unauthorized.into()),
        409 => Err(UploadFailure::Conflict.into()),
        400..=499 => Err(UploadFailure::Rejected.into()),
        _ => Err(UploadFailure::Unavailable.into()),
    }
}

/// Collect every record of a day directory in path order, refusing a day
/// whose paths the record format cannot carry.
pub fn collect_day(directory: &Path) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    collect(directory, directory, &mut records)?;
    records.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(records)
}

fn collect(root: &Path, directory: &Path, records: &mut Vec<Record>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = relative_path(root, &path)?;
        if directory == root && CAPTURE_DIRECTORIES.contains(&relative.as_str()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.ends_with(".tmp") {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!("day contains a link");
        }
        if file_type.is_dir() {
            collect(root, &path, records)?;
        } else if file_type.is_file() {
            ensure!(
                valid_record_path(&relative),
                "day contains a path the record format cannot carry"
            );
            let bytes = fs::read(&path)?;
            let sha256 = hex(&Sha256::digest(&bytes));
            records.push(Record {
                path: relative,
                bytes,
                sha256,
            });
        }
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| anyhow!("record escaped its day"))?;
    let segments: Option<Vec<&str>> = relative
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect();
    segments
        .map(|segments| segments.join("/"))
        .ok_or_else(|| anyhow!("day contains a path that is not UTF-8"))
}

/// A record path is 1 to 8 `/`-separated segments of ASCII letters, digits,
/// `.`, `_`, and `-`, each at most 128 bytes and none of them `.` or `..`.
pub fn valid_record_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    segments.len() <= MAX_SEGMENTS
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.len() <= MAX_SEGMENT_BYTES
                && *segment != "."
                && *segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn day_directories(root: &Path) -> Result<Vec<(NaiveDate, PathBuf)>> {
    let mut days = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if let Ok(day) = NaiveDate::parse_from_str(&name, "%Y-%m-%d")
            && day.format("%Y-%m-%d").to_string() == name
        {
            days.push((day, entry.path()));
        }
    }
    days.sort();
    Ok(days)
}

fn read_ledger(root: &Path) -> Result<Ledger> {
    let path = root.join(LEDGER_FILE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("the upload ledger is malformed"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(error) => Err(error).context("the upload ledger is unreadable"),
    }
}

fn write_ledger(root: &Path, ledger: &Ledger) -> Result<()> {
    let _guard = crate::artifact_store::lock(root)?;
    let path = root.join(LEDGER_FILE);
    let temporary = root.join(format!("{LEDGER_FILE}.tmp"));
    fs::write(&temporary, serde_json::to_vec_pretty(ledger)?)?;
    fs::rename(&temporary, &path).context("failed to replace the upload ledger")?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod unit {
    use super::{expired, valid_record_path};
    use chrono::NaiveDate;

    #[test]
    fn record_paths_are_bounded_portable_segments() {
        assert!(valid_record_path("blocks/2026-10-30T0910.json"));
        assert!(valid_record_path("daily.md"));
        assert!(!valid_record_path("../daily.md"));
        assert!(!valid_record_path("blocks//x.json"));
        assert!(!valid_record_path("blocks/x y.json"));
        assert!(!valid_record_path(&["a"; 9].join("/")));
        assert!(!valid_record_path(&"a".repeat(129)));
    }

    #[test]
    fn a_day_expires_after_the_retention_window() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        assert!(!expired(
            NaiveDate::from_ymd_opt(2026, 11, 3).unwrap(),
            today,
            7
        ));
        assert!(expired(
            NaiveDate::from_ymd_opt(2026, 11, 2).unwrap(),
            today,
            7
        ));
    }
}

#[cfg(test)]
mod component {
    use super::{UploadFailure, scan_once};
    use crate::config::AppConfig;
    use chrono::NaiveDate;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const DAY: &str = "2020-01-01";

    /// Requests the fake server received: path, then headers by lowercase name.
    type Received = Arc<Mutex<Vec<(String, BTreeMap<String, String>)>>>;

    /// A loopback stand-in for the server's records endpoint that answers
    /// each PUT with the status `respond` picks for its path.
    async fn fake_server(respond: fn(&str) -> u16) -> (String, Received) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let received: Received = Arc::default();
        let log = Arc::clone(&received);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    loop {
                        let mut line = String::new();
                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                        let mut headers = BTreeMap::new();
                        loop {
                            let mut header = String::new();
                            stream.read_line(&mut header).await.unwrap();
                            let header = header.trim_end();
                            if header.is_empty() {
                                break;
                            }
                            let (name, value) = header.split_once(':').unwrap();
                            headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
                        }
                        let length = headers
                            .get("content-length")
                            .map_or(0, |length| length.parse().unwrap());
                        let mut body = vec![0; length];
                        stream.read_exact(&mut body).await.unwrap();
                        headers.insert("body-sha256".to_string(), hex(&body));
                        let status = respond(&path);
                        log.lock().unwrap().push((path, headers));
                        let response = format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\n\r\n");
                        stream
                            .get_mut()
                            .write_all(response.as_bytes())
                            .await
                            .unwrap();
                    }
                });
            }
        });
        (origin, received)
    }

    fn hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    struct DataDir(PathBuf);

    impl DataDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ashe-worker-component-{label}-{}-{}",
                std::process::id(),
                unique_suffix()
            ));
            let day = path.join(DAY);
            fs::create_dir_all(day.join("blocks")).unwrap();
            fs::create_dir_all(day.join("frames")).unwrap();
            fs::write(day.join("blocks").join("0000.json"), b"{\"block\":1}").unwrap();
            fs::write(day.join("journal.md"), b"journal").unwrap();
            fs::write(day.join("frames").join("0000.png"), b"capture").unwrap();
            Self(path)
        }

        fn day(&self) -> PathBuf {
            self.0.join(DAY)
        }
    }

    impl Drop for DataDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn config(root: &Path, server_url: String) -> AppConfig {
        let mut config = AppConfig::synthetic(root.to_path_buf());
        config.daily_report_enabled = false;
        config.server_url = server_url;
        config.local_retention_days = 3;
        config
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2020, 1, 10).unwrap()
    }

    #[tokio::test]
    async fn a_fully_acknowledged_day_is_uploaded_without_captures_then_removed() {
        let data = DataDir::new("acknowledged");
        let (origin, received) = fake_server(|_| 201).await;
        let config = config(&data.0, origin);
        let outcome = scan_once(&config, &reqwest::Client::new(), today())
            .await
            .unwrap();

        assert_eq!((outcome.uploaded, outcome.removed_days), (2, 1));
        assert!(!data.day().exists());
        let received = received.lock().unwrap();
        let paths: Vec<&str> = received.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/v1/records/2020-01-01/blocks/0000.json",
                "/v1/records/2020-01-01/journal.md"
            ]
        );
        for (_, headers) in received.iter() {
            assert_eq!(headers["authorization"], "Bearer synthetic-server-token");
            assert_eq!(headers["x-ashe-record-format"], "1");
            assert_eq!(headers["x-ashe-record-sha256"], headers["body-sha256"]);
        }
    }

    #[tokio::test]
    async fn a_conflict_stops_the_day_and_keeps_it() {
        let data = DataDir::new("conflict");
        let (origin, received) =
            fake_server(|path| if path.contains("blocks") { 409 } else { 201 }).await;
        let config = config(&data.0, origin);
        let error = scan_once(&config, &reqwest::Client::new(), today())
            .await
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref(),
            Some(UploadFailure::Conflict)
        ));
        assert!(data.day().join("journal.md").is_file());
        assert_eq!(received.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_server_never_removes_data() {
        let data = DataDir::new("unreachable");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let config = config(&data.0, origin);
        let error = scan_once(&config, &reqwest::Client::new(), today())
            .await
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref(),
            Some(UploadFailure::Unavailable)
        ));
        assert!(data.day().join("blocks").join("0000.json").is_file());
    }
}

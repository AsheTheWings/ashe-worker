use crate::activity_telemetry::{self, ActivitySample};
use crate::block_artifact::{
    ActivityNarrative, BLOCK_SCHEMA_VERSION, BlockArtifact, BlockDocumentInput,
};
use crate::config::AppConfig;
use crate::daily_report::DailyReportHandle;
use crate::llm_client;
use crate::logger;
use crate::screen_capture;
use anyhow::{Context, Result, anyhow};
use chrono::{Local, NaiveDateTime, TimeZone};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PENDING_BLOCK_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug)]
pub struct ActivityStatus {
    pub running: bool,
    pub summary: String,
    pub current_frames: usize,
}

#[derive(Clone)]
pub struct ActivityHandle {
    tx: Sender<ActivityCommand>,
    status: Arc<Mutex<ActivityStatus>>,
    artifacts: PathBuf,
}

enum ActivityCommand {
    Toggle,
    Shutdown,
}

struct DescriptionQueue {
    tx: Sender<Block>,
    active: Arc<Mutex<HashSet<String>>>,
}

impl DescriptionQueue {
    fn spawn(config: AppConfig, status: Arc<Mutex<ActivityStatus>>) -> Self {
        Self::spawn_with_processor(move |block| describe_and_store(&config, block, &status))
    }

    fn spawn_with_processor<F>(process: F) -> Self
    where
        F: Fn(Block) + Send + 'static,
    {
        let (tx, rx) = crossbeam_channel::unbounded::<Block>();
        let active = Arc::new(Mutex::new(HashSet::new()));
        let worker_active = Arc::clone(&active);
        std::thread::spawn(move || {
            while let Ok(block) = rx.recv() {
                let id = block.id();
                process(block);
                worker_active
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&id);
            }
        });
        Self { tx, active }
    }

    fn enqueue(&self, block: Block) -> Result<()> {
        let id = block.id();
        {
            let mut active = self
                .active
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !active.insert(id.clone()) {
                return Ok(());
            }
        }
        if self.tx.send(block).is_err() {
            self.active
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&id);
            return Err(anyhow!("activity description worker stopped"));
        }
        Ok(())
    }
}

impl ActivityHandle {
    pub fn spawn(config: AppConfig) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let status = Arc::new(Mutex::new(ActivityStatus {
            running: config.activity_enabled,
            summary: if config.activity_enabled {
                "activity tracking starting".to_string()
            } else {
                "activity tracking paused".to_string()
            },
            current_frames: 0,
        }));
        let worker_status = Arc::clone(&status);
        let artifacts = config.activity_artifacts_dir.clone();
        std::thread::spawn(move || {
            if let Err(error) = run(config, rx, &worker_status) {
                logger::info(format!("Activity tracking stopped with error: {error:#}"));
                set_status(&worker_status, false, format!("activity error: {error}"), 0);
            }
        });
        Self {
            tx,
            status,
            artifacts,
        }
    }

    pub fn toggle(&self) {
        let _ = self.tx.send(ActivityCommand::Toggle);
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(ActivityCommand::Shutdown);
    }

    pub fn status(&self) -> ActivityStatus {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn artifacts_dir(&self) -> &Path {
        &self.artifacts
    }

    pub fn today_journal(&self) -> PathBuf {
        self.artifacts
            .join(Local::now().format("%Y-%m-%d").to_string())
            .join("journal.md")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FrameRecord {
    path: String,
    ts: i64,
    monitor: i32,
    width: u32,
    height: u32,
    delta: f32,
    window: String,
    sent_to_model: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Block {
    schema_version: u32,
    start: i64,
    end: i64,
    day: String,
    frames: Vec<FrameRecord>,
    samples: Vec<ActivitySample>,
    captured: usize,
    attempts: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ContextState {
    #[serde(default)]
    summary_through: String,
}

#[derive(Clone, Debug)]
struct StoredReport {
    id: String,
    block: BlockDocumentInput,
}

struct BlockMetrics {
    timeline: Vec<String>,
    totals: String,
    app_seconds: BTreeMap<String, u64>,
    active_seconds: u64,
    idle_seconds: u64,
}

#[derive(Default)]
struct DedupState {
    fingerprint: Vec<u8>,
    retained_ts: Option<i64>,
}

impl DedupState {
    fn clear(&mut self) {
        self.fingerprint.clear();
        self.retained_ts = None;
    }

    fn observe(&mut self, fingerprint: Vec<u8>, ts: i64, threshold: f32, max_gap_s: u64) -> f32 {
        let first = self.fingerprint.is_empty();
        let delta = screen_capture::changed_percent(&self.fingerprint, &fingerprint);
        let gap_elapsed = self
            .retained_ts
            .is_some_and(|previous| ts - previous >= max_gap_s as i64);
        if first || delta >= threshold || gap_elapsed {
            self.fingerprint = fingerprint;
            self.retained_ts = Some(ts);
        }
        delta
    }
}

impl Block {
    fn new(start: i64, seconds: i64) -> Self {
        Self {
            schema_version: PENDING_BLOCK_SCHEMA_VERSION,
            start,
            end: start + seconds,
            day: day_key(start),
            frames: Vec::new(),
            samples: Vec::new(),
            captured: 0,
            attempts: 0,
        }
    }

    fn id(&self) -> String {
        format!("{}T{}", self.day, clock(self.start, "%H%M"))
    }

    fn slug(&self) -> String {
        format!("{}-{}", clock(self.start, "%H%M"), clock(self.end, "%H%M"))
    }

    fn label(&self) -> String {
        format!(
            "{}-{}",
            clock(self.start, "%H:%M"),
            clock(self.end, "%H:%M")
        )
    }
}

fn run(
    config: AppConfig,
    commands: Receiver<ActivityCommand>,
    status: &Arc<Mutex<ActivityStatus>>,
) -> Result<()> {
    prepare_store(&config.activity_artifacts_dir)?;
    let _daily_reports = DailyReportHandle::spawn(config.clone());
    let descriptions = DescriptionQueue::spawn(config.clone(), Arc::clone(status));
    let mut running = config.activity_enabled;
    let block_seconds = (config.activity_block_minutes * 60) as i64;
    let mut block: Option<Block> = None;
    let mut dedup_state = DedupState::default();
    let mut next_capture = now();
    let mut was_inactive = false;

    if running {
        block = recover_pending(&config, block_seconds, &descriptions)?;
        logger::info(format!(
            "Activity tracking started: capture={}s max_frame_gap={}s block={}m dedup={}pct idle={}s min_active={}s artifacts={}",
            config.activity_capture_interval,
            config.activity_max_frame_gap_s,
            config.activity_block_minutes,
            config.activity_dedup_threshold,
            config.activity_idle_threshold_s,
            config.activity_min_active_seconds,
            config.activity_artifacts_dir.display()
        ));
    }

    loop {
        match commands.recv_timeout(Duration::from_millis(config.activity_telemetry_interval_ms)) {
            Ok(ActivityCommand::Shutdown) => {
                if let Some(current) = block.as_ref() {
                    save_pending(&config.activity_artifacts_dir, current)?;
                }
                set_status(status, false, "activity tracking stopped".to_string(), 0);
                return Ok(());
            }
            Ok(ActivityCommand::Toggle) => {
                if running {
                    if let Some(current) = block.as_ref() {
                        save_pending(&config.activity_artifacts_dir, current)?;
                    }
                    dedup_state.clear();
                    running = false;
                    set_status(status, false, "activity tracking paused".to_string(), 0);
                } else {
                    running = true;
                    block = recover_pending(&config, block_seconds, &descriptions)?;
                    dedup_state.clear();
                    next_capture = now();
                    was_inactive = false;
                    set_status(status, true, "activity tracking resumed".to_string(), 0);
                }
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        if !running {
            continue;
        }

        let stamp = now();
        let boundary = stamp - stamp.rem_euclid(block_seconds);
        if block
            .as_ref()
            .is_some_and(|current| boundary >= current.end)
        {
            let finished = block.take().expect("checked above");
            save_pending(&config.activity_artifacts_dir, &finished)?;
            descriptions.enqueue(finished)?;
            dedup_state.clear();
        }
        let current = block.get_or_insert_with(|| Block::new(boundary, block_seconds));
        let sample = activity_telemetry::sample(stamp);
        current.samples.push(sample.clone());

        let inactive = sample.locked || sample.idle_s >= config.activity_idle_threshold_s;
        if was_inactive && !inactive {
            // Do not make the user wait for a stale idle/locked schedule after input resumes.
            next_capture = stamp;
        }
        was_inactive = inactive;

        let Some(capture_interval) =
            capture_interval_for_sample(&sample, config.activity_capture_interval)
        else {
            // Sampling continues for accurate coverage, but locked desktops are never captured.
            next_capture = stamp + config.activity_capture_interval as i64;
            continue;
        };

        if stamp >= next_capture {
            next_capture = stamp + capture_interval as i64;
            if !denied(&config, &sample) {
                match capture(&config, current, &mut dedup_state, &sample) {
                    Ok(()) => {
                        save_pending(&config.activity_artifacts_dir, current)?;
                        set_status(
                            status,
                            true,
                            format!("recording {}", current.label()),
                            current.frames.len(),
                        );
                    }
                    Err(error) => {
                        logger::info(format!("Activity capture failed: {error:#}"));
                        set_status(
                            status,
                            true,
                            format!("capture failed: {error}"),
                            current.frames.len(),
                        );
                    }
                }
            }
            purge_expired_frames(&config)?;
        }
    }
}

fn capture(
    config: &AppConfig,
    block: &mut Block,
    dedup_state: &mut DedupState,
    sample: &ActivitySample,
) -> Result<()> {
    let frame = screen_capture::capture_screen(config.activity_monitor == 0)?;
    let stamp = now();
    let delta = dedup_state.observe(
        frame.fingerprint,
        stamp,
        config.activity_dedup_threshold,
        config.activity_max_frame_gap_s,
    );
    let directory = day_dir(&config.activity_artifacts_dir, &block.day).join("frames");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{}_m{}.webp", clock(stamp, "%H%M%S"), 1));
    fs::write(&path, frame.webp).context("failed to save activity frame")?;
    block.frames.push(FrameRecord {
        path: path.display().to_string(),
        ts: stamp,
        monitor: config.activity_monitor,
        width: frame.width,
        height: frame.height,
        delta,
        window: sample.label(),
        sent_to_model: false,
    });
    block.captured += 1;
    Ok(())
}

fn denied(config: &AppConfig, sample: &ActivitySample) -> bool {
    matches_denylist(&config.activity_denylist, sample)
}

fn matches_denylist(denylist: &[String], sample: &ActivitySample) -> bool {
    if denylist.is_empty() {
        return false;
    }
    let text = format!("{} {}", sample.exe, sample.title).to_ascii_lowercase();
    denylist.iter().any(|term| text.contains(term))
}

fn capture_interval_for_sample(sample: &ActivitySample, capture_interval_s: u64) -> Option<u64> {
    if sample.locked {
        None
    } else {
        Some(capture_interval_s)
    }
}

fn should_suppress_description(
    active_seconds: u64,
    distinct_frames: usize,
    min_active_seconds: u64,
) -> bool {
    active_seconds < min_active_seconds && distinct_frames <= 1
}

fn describe_and_store(config: &AppConfig, mut block: Block, status: &Arc<Mutex<ActivityStatus>>) {
    let metrics = block_metrics(config, &block);
    if block.frames.is_empty() {
        let outcome = terminal_outcome(&block);
        let reason = if outcome == "locked" {
            "workstation locked"
        } else {
            "no captured activity"
        };
        let _ = write_terminal_block(config, &block, outcome, reason, &metrics);
        let _ = clear_pending(&config.activity_artifacts_dir, &block);
        return;
    }
    let distinct_frames = block
        .frames
        .iter()
        .filter(|frame| frame.delta >= config.activity_dedup_threshold)
        .count();
    if should_suppress_description(
        metrics.active_seconds,
        distinct_frames,
        config.activity_min_active_seconds,
    ) {
        let outcome = terminal_outcome(&block);
        let reason = if outcome == "locked" {
            "workstation locked"
        } else {
            "idle or unchanged"
        };
        let _ = write_terminal_block(config, &block, outcome, reason, &metrics);
        let _ = clear_pending(&config.activity_artifacts_dir, &block);
        return;
    }

    set_work_status(
        status,
        format!("describing {}", block.label()),
        block.frames.len(),
    );
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build activity runtime")
    {
        Ok(runtime) => runtime,
        Err(error) => {
            retry_description(config, &mut block, status, &error);
            return;
        }
    };
    let nominal_duration_s = (block.end - block.start).max(1) as u64;
    let duration_budget_s = metrics
        .active_seconds
        .saturating_add(metrics.idle_seconds)
        .clamp(1, nominal_duration_s);

    let (paths, images) = {
        let selected = select_frames(&block.frames, config);
        readable_images(selected)
    };
    if images.is_empty() {
        let reason = "no selected frame remained readable";
        if let Err(error) =
            write_terminal_block(config, &block, "insufficient_evidence", reason, &metrics)
        {
            logger::info(format!(
                "Writing insufficient-evidence block {} failed: {error:#}",
                block.id()
            ));
            return;
        }
        let _ = clear_pending(&config.activity_artifacts_dir, &block);
        set_work_status(
            status,
            "activity block lacked readable evidence".to_string(),
            0,
        );
        return;
    }
    for frame in &mut block.frames {
        frame.sent_to_model = paths.contains(&frame.path);
    }
    let result = runtime.block_on(async {
        let earlier_context = build_earlier_context(config, block.start).await?;
        let context = description_context(
            config,
            &block,
            &metrics,
            nominal_duration_s,
            duration_budget_s,
            images.len(),
            &earlier_context,
        );
        llm_client::describe_activity_block(config.clone(), context, images, duration_budget_s)
            .await
    });
    match result {
        Ok(narrative) => {
            finish_description(config, &block, &narrative, &metrics, paths.len(), status)
        }
        Err(error)
            if error
                .downcast_ref::<llm_client::InvalidActivityOutput>()
                .is_some() =>
        {
            let reason = sanitized_error(&error);
            if let Err(write_error) =
                write_terminal_block(config, &block, "invalid_model_output", &reason, &metrics)
            {
                logger::info(format!(
                    "Writing invalid-output block {} failed: {write_error:#}",
                    block.id()
                ));
                return;
            }
            let _ = clear_pending(&config.activity_artifacts_dir, &block);
            logger::info(format!(
                "Description of {} produced invalid structured output",
                block.id()
            ));
            set_work_status(
                status,
                "activity block had invalid model output".to_string(),
                0,
            );
        }
        Err(error) => retry_description(config, &mut block, status, &error),
    }
}

fn finish_description(
    config: &AppConfig,
    block: &Block,
    narrative: &ActivityNarrative,
    metrics: &BlockMetrics,
    frames_sent: usize,
    status: &Arc<Mutex<ActivityStatus>>,
) {
    let title = narrative.title.clone();
    if let Err(error) = write_report(config, block, narrative, metrics, frames_sent) {
        logger::info(format!("Writing activity block failed: {error:#}"));
        return;
    }
    let _ = clear_pending(&config.activity_artifacts_dir, block);
    set_work_status(status, format!("recorded: {title}"), 0);
}

fn retry_description(
    config: &AppConfig,
    block: &mut Block,
    status: &Arc<Mutex<ActivityStatus>>,
    _error: &anyhow::Error,
) {
    block.attempts += 1;
    let _ = save_pending(&config.activity_artifacts_dir, block);
    logger::info(format!(
        "Activity description block={} attempt={} failed; retained for retry",
        block.id(),
        block.attempts
    ));
    set_work_status(
        status,
        "activity description pending: request failed".to_string(),
        block.frames.len(),
    );
}

fn description_context(
    config: &AppConfig,
    block: &Block,
    metrics: &BlockMetrics,
    nominal_duration_s: u64,
    duration_budget_s: u64,
    image_count: usize,
    earlier_context: &str,
) -> String {
    format!(
        "## Block {} on {}\nThis block spans {} seconds with {} seconds of measured coverage. Captured {} temporary screenshots; {} selected images are attached in chronological order. The evidence capture policy targets every {} seconds while the workstation is unlocked, and visual deduplication retains a frame at least every {} seconds.\nMeasured app time: {}.\n\n### Measured focus timeline (ground truth)\n{}\n\n{}\n\nProduce the unified activity report and independently validated learning units for this block only.",
        block.label(),
        block.day,
        nominal_duration_s,
        duration_budget_s,
        block.captured,
        image_count,
        config.activity_capture_interval,
        config.activity_max_frame_gap_s,
        if metrics.totals.is_empty() {
            "unavailable"
        } else {
            &metrics.totals
        },
        metrics
            .timeline
            .iter()
            .map(|line| format!("- {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        earlier_context,
    )
}

fn sanitized_error(error: &anyhow::Error) -> String {
    error
        .to_string()
        .replace(['\r', '\n'], " ")
        .chars()
        .take(500)
        .collect()
}

fn readable_images(frames: Vec<&FrameRecord>) -> (HashSet<String>, Vec<(String, Vec<u8>)>) {
    let mut paths = HashSet::new();
    let mut images = Vec::new();
    for frame in frames {
        let Ok(bytes) = fs::read(&frame.path) else {
            continue;
        };
        paths.insert(frame.path.clone());
        images.push((
            format!(
                "{} · monitor {} · {}x{} · foreground: {}",
                clock(frame.ts, "%H:%M:%S"),
                frame.monitor,
                frame.width,
                frame.height,
                frame.window
            ),
            bytes,
        ));
    }
    (paths, images)
}

fn select_frames<'a>(frames: &'a [FrameRecord], config: &AppConfig) -> Vec<&'a FrameRecord> {
    select_frame_candidates(
        frames.iter().collect(),
        config.activity_max_frame_gap_s,
        config.activity_dedup_threshold,
        config.activity_max_frames_per_call,
        config.activity_max_payload_mb,
    )
}

fn select_frame_candidates(
    candidates: Vec<&FrameRecord>,
    max_gap_s: u64,
    dedup_threshold: f32,
    max_frames_per_call: usize,
    max_payload_mb: f32,
) -> Vec<&FrameRecord> {
    select_frame_candidates_with_size(
        candidates,
        max_gap_s,
        dedup_threshold,
        max_frames_per_call,
        max_payload_mb,
        |frame| {
            fs::metadata(&frame.path)
                .ok()
                .map(|metadata| metadata.len())
        },
    )
}

fn select_frame_candidates_with_size<F>(
    candidates: Vec<&FrameRecord>,
    max_gap_s: u64,
    dedup_threshold: f32,
    max_frames_per_call: usize,
    max_payload_mb: f32,
    size_of: F,
) -> Vec<&FrameRecord>
where
    F: Fn(&FrameRecord) -> Option<u64>,
{
    let mut kept = Vec::new();
    for (index, frame) in candidates.iter().enumerate() {
        let keep_for_gap = kept
            .last()
            .is_some_and(|previous: &&FrameRecord| frame.ts - previous.ts >= max_gap_s as i64);
        if index == 0 || frame.delta >= dedup_threshold || keep_for_gap {
            kept.push(*frame);
        }
    }
    if let Some(last) = candidates.last()
        && kept.last().is_none_or(|frame| frame.path != last.path)
    {
        kept.push(*last);
    }
    while kept.len() > max_frames_per_call {
        let index = kept[1..kept.len() - 1]
            .iter()
            .enumerate()
            .min_by(|left, right| left.1.delta.total_cmp(&right.1.delta))
            .map(|(index, _)| index + 1)
            .unwrap_or(kept.len() - 1);
        kept.remove(index);
    }
    let budget = (max_payload_mb * 1_000_000.0 * 0.72) as u64;
    while kept.len() > 2 && kept.iter().filter_map(|frame| size_of(frame)).sum::<u64>() > budget {
        let index = kept[1..kept.len() - 1]
            .iter()
            .enumerate()
            .min_by(|left, right| left.1.delta.total_cmp(&right.1.delta))
            .map(|(index, _)| index + 1)
            .unwrap_or(1);
        kept.remove(index);
    }
    kept
}

fn recover_pending(
    config: &AppConfig,
    block_seconds: i64,
    descriptions: &DescriptionQueue,
) -> Result<Option<Block>> {
    let pending = config.activity_artifacts_dir.join("pending");
    let stamp = now();
    let boundary = stamp - stamp.rem_euclid(block_seconds);
    let mut current = None;
    let mut recovered = fs::read_dir(pending)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .filter_map(|path| {
            let bytes = fs::read(&path).ok()?;
            let block = serde_json::from_slice::<Block>(&bytes).ok()?;
            if block.schema_version != PENDING_BLOCK_SCHEMA_VERSION {
                return None;
            }
            Some((path, block))
        })
        .collect::<Vec<_>>();
    recovered.sort_by_key(|(_, block)| block.start);
    for (path, block) in recovered {
        if block.start == boundary {
            current = Some(block);
        } else if !report_path(&config.activity_artifacts_dir, &block).is_file() {
            descriptions.enqueue(block)?;
        } else {
            let directory = day_dir(&config.activity_artifacts_dir, &block.day);
            rebuild_journal(&directory)?;
            let _ = fs::remove_file(path);
        }
    }
    Ok(current)
}

fn prepare_store(root: &Path) -> Result<()> {
    fs::create_dir_all(root.join("pending"))?;
    Ok(())
}

fn day_dir(root: &Path, day: &str) -> PathBuf {
    root.join(day)
}

fn pending_path(root: &Path, block: &Block) -> PathBuf {
    root.join("pending").join(format!("{}.json", block.id()))
}

fn report_path(root: &Path, block: &Block) -> PathBuf {
    day_dir(root, &block.day)
        .join("blocks")
        .join(format!("{}.json", block.slug()))
}

fn save_pending(root: &Path, block: &Block) -> Result<()> {
    let _write_guard = crate::artifact_store::lock(root)?;
    let path = pending_path(root, block);
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(block)?)?;
    for attempt in 0..5 {
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
        match fs::rename(&temporary, &path) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < 4 => {
                logger::info(format!(
                    "Pending manifest replace retry {}: {error}",
                    attempt + 1
                ));
                std::thread::sleep(Duration::from_millis(100 * (attempt + 1)));
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("pending manifest retry loop always returns")
}

fn clear_pending(root: &Path, block: &Block) -> Result<()> {
    let _write_guard = crate::artifact_store::lock(root)?;
    let path = pending_path(root, block);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn write_report(
    config: &AppConfig,
    block: &Block,
    narrative: &ActivityNarrative,
    metrics: &BlockMetrics,
    frames_sent: usize,
) -> Result<()> {
    let _write_guard = crate::artifact_store::lock(&config.activity_artifacts_dir)?;
    let directory = day_dir(&config.activity_artifacts_dir, &block.day);
    fs::create_dir_all(directory.join("blocks"))?;
    fs::create_dir_all(directory.join("keyframes"))?;
    let keyframe = block
        .frames
        .iter()
        .find(|frame| frame.sent_to_model)
        .or_else(|| block.frames.first());
    let keyframe_relative = if let Some(frame) = keyframe {
        let target = directory.join("keyframes").join(format!(
            "{}_{}.webp",
            block.slug(),
            clock(frame.ts, "%H%M%S")
        ));
        fs::copy(&frame.path, &target)?;
        format!(
            "keyframes/{}",
            target
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
        )
    } else {
        String::new()
    };
    let artifact = BlockArtifact {
        schema_version: BLOCK_SCHEMA_VERSION,
        block: block.id(),
        window_start: block.start,
        window_end: block.end,
        outcome: "described".to_string(),
        active_seconds: metrics.active_seconds,
        idle_seconds: metrics.idle_seconds,
        app_seconds: metrics.app_seconds.clone(),
        timeline: metrics.timeline.clone(),
        frames_captured: block.captured,
        frames_sent,
        keyframe: (!keyframe_relative.is_empty()).then_some(keyframe_relative),
        model: Some(config.journal_model.clone()),
        title: Some(narrative.title.clone()),
        report: Some(narrative.report.clone()),
        subjects: narrative.subjects.clone(),
        learning_subjects: narrative.learning_subjects.clone(),
        reason: None,
        error: None,
    };
    write_block_artifact(
        &report_path(&config.activity_artifacts_dir, block),
        &artifact,
    )?;
    rebuild_journal(&directory)?;
    Ok(())
}

fn rebuild_journal(directory: &Path) -> Result<()> {
    let mut entries = BTreeMap::new();
    let paths = fs::read_dir(directory.join("blocks"))?
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    for path in paths
        .iter()
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
    {
        let Ok(bytes) = fs::read(path) else { continue };
        let Ok(artifact) = serde_json::from_slice::<BlockArtifact>(&bytes) else {
            continue;
        };
        if let Some((id, entry)) = json_journal_entry(&artifact) {
            entries.insert(id, entry);
        }
    }
    let path = directory.join("journal.md");
    let temporary = path.with_extension("md.tmp");
    let content = entries.into_values().collect::<Vec<_>>().join("\n");
    fs::write(&temporary, content)?;
    replace_file(&temporary, &path)
}

fn json_journal_entry(artifact: &BlockArtifact) -> Option<(String, String)> {
    if !artifact.has_current_schema() || artifact.outcome != "described" {
        return None;
    }
    let (Some(title), Some(report)) = (&artifact.title, &artifact.report) else {
        return None;
    };
    Some((
        artifact.block.clone(),
        format!(
            "## {}-{} — {}\n\n{}\n",
            clock(artifact.window_start, "%H:%M"),
            clock(artifact.window_end, "%H:%M"),
            title,
            report.trim(),
        ),
    ))
}

fn write_terminal_block(
    config: &AppConfig,
    block: &Block,
    outcome: &str,
    reason: &str,
    metrics: &BlockMetrics,
) -> Result<()> {
    let _write_guard = crate::artifact_store::lock(&config.activity_artifacts_dir)?;
    let directory = day_dir(&config.activity_artifacts_dir, &block.day);
    fs::create_dir_all(directory.join("blocks"))?;
    let path = report_path(&config.activity_artifacts_dir, block);
    let artifact = BlockArtifact {
        schema_version: BLOCK_SCHEMA_VERSION,
        block: block.id(),
        window_start: block.start,
        window_end: block.end,
        outcome: outcome.to_string(),
        active_seconds: metrics.active_seconds,
        idle_seconds: metrics.idle_seconds,
        app_seconds: metrics.app_seconds.clone(),
        timeline: metrics.timeline.clone(),
        frames_captured: block.captured,
        frames_sent: 0,
        keyframe: None,
        model: None,
        title: None,
        report: None,
        subjects: Vec::new(),
        learning_subjects: Vec::new(),
        reason: Some(reason.to_string()),
        error: (outcome == "invalid_model_output").then_some(reason.to_string()),
    };
    write_block_artifact(&path, &artifact)?;
    rebuild_journal(&directory)
}

fn write_block_artifact(path: &Path, artifact: &BlockArtifact) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(artifact)?)?;
    replace_file(&temporary, path)
}

fn block_metrics(config: &AppConfig, block: &Block) -> BlockMetrics {
    let interval = config.activity_telemetry_interval_ms as f64 / 1000.0;
    let timeline =
        activity_telemetry::timeline(&block.samples, interval, config.activity_idle_threshold_s);
    let totals = activity_telemetry::app_totals(&block.samples, interval);
    let app_seconds = activity_telemetry::app_total_values(&block.samples, interval)
        .into_iter()
        .map(|(name, seconds)| (name, seconds.round().max(0.0) as u64))
        .collect();
    let active_samples = block
        .samples
        .iter()
        .filter(|sample| !sample.locked && sample.idle_s < config.activity_idle_threshold_s)
        .count() as f64;
    let total_seconds = block.samples.len() as f64 * interval;
    let active_seconds = (active_samples * interval).round().max(0.0) as u64;
    BlockMetrics {
        timeline,
        totals,
        app_seconds,
        active_seconds,
        idle_seconds: total_seconds.round().max(0.0) as u64 - active_seconds,
    }
}

fn terminal_outcome(block: &Block) -> &'static str {
    if !block.samples.is_empty() && block.samples.iter().all(|sample| sample.locked) {
        "locked"
    } else {
        "idle"
    }
}

async fn build_earlier_context(config: &AppConfig, before: i64) -> Result<String> {
    const SUMMARY_BATCH_SIZE: usize = 24;

    let root = &config.activity_artifacts_dir;
    let reports = load_successful_reports(root, before)?;
    let mut state = read_context_state(root);
    let unsummarized = reports
        .iter()
        .filter(|report| report.id.as_str() > state.summary_through.as_str())
        .collect::<Vec<_>>();
    let detailed_offset =
        context_detailed_offset(unsummarized.len(), config.activity_context_blocks);
    let aged = &unsummarized[..detailed_offset];
    let detailed = &unsummarized[detailed_offset..];

    let stored_summary = read_rolling_summary(root);
    let mut summary = truncate_chars(&stored_summary, config.activity_context_summary_max_chars);
    if summary != stored_summary {
        write_rolling_summary(root, &summary, &state.summary_through)?;
    }
    for batch in aged.chunks(SUMMARY_BATCH_SIZE) {
        let input = serde_json::json!({
            "previous_summary": if summary.trim().is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(summary.trim().to_string())
            },
            "blocks": batch
                .iter()
                .map(|report| &report.block)
                .collect::<Vec<_>>(),
        });
        summary = llm_client::refresh_activity_summary(
            config.clone(),
            input,
            config.activity_context_summary_max_chars,
        )
        .await?;
        state.summary_through = batch
            .last()
            .map(|report| report.id.clone())
            .unwrap_or_else(|| state.summary_through.clone());
        write_rolling_summary(root, &summary, &state.summary_through)?;
        write_context_state(root, &state)?;
    }

    let context = serde_json::json!({
        "rolling_summary": if summary.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(summary.trim().to_string())
        },
        "rolling_summary_through": if state.summary_through.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(state.summary_through.clone())
        },
        "recent_blocks": detailed
            .iter()
            .map(|report| &report.block)
            .collect::<Vec<_>>(),
    });
    Ok(format!(
        "### Earlier context (structured JSON)\n{}",
        serde_json::to_string_pretty(&context)?,
    ))
}

fn context_detailed_offset(report_count: usize, context_blocks: usize) -> usize {
    report_count.saturating_sub(context_blocks)
}

fn load_successful_reports(root: &Path, before: i64) -> Result<Vec<StoredReport>> {
    let mut reports = BTreeMap::new();
    for day in fs::read_dir(root)? {
        let day = day?;
        if !day.path().is_dir() {
            continue;
        }
        let blocks = day.path().join("blocks");
        let Ok(entries) = fs::read_dir(blocks) else {
            continue;
        };
        for path in entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        {
            let Ok(bytes) = fs::read(path) else { continue };
            let Ok(artifact) = serde_json::from_slice::<BlockArtifact>(&bytes) else {
                continue;
            };
            if !artifact.has_current_schema()
                || artifact.outcome != "described"
                || artifact.report.is_none()
            {
                continue;
            }
            let Some(start) = block_timestamp(&artifact.block) else {
                continue;
            };
            if start < before {
                reports.insert(
                    artifact.block.clone(),
                    StoredReport {
                        id: artifact.block.clone(),
                        block: artifact.document_input(),
                    },
                );
            }
        }
    }
    Ok(reports.into_values().collect())
}

fn block_timestamp(id: &str) -> Option<i64> {
    let naive = NaiveDateTime::parse_from_str(id, "%Y-%m-%dT%H%M").ok()?;
    Local
        .from_local_datetime(&naive)
        .single()
        .or_else(|| Local.from_local_datetime(&naive).earliest())
        .map(|stamp| stamp.timestamp())
}

fn context_state_path(root: &Path) -> PathBuf {
    root.join("context-state.json")
}

fn read_context_state(root: &Path) -> ContextState {
    fs::read(context_state_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_context_state(root: &Path, state: &ContextState) -> Result<()> {
    let path = context_state_path(root);
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(state)?)?;
    replace_file(&temporary, &path)
}

fn read_rolling_summary(root: &Path) -> String {
    let Ok(content) = fs::read_to_string(root.join("summary.md")) else {
        return String::new();
    };
    content
        .split_once("\n\n")
        .map(|(_, body)| body.trim().to_string())
        .unwrap_or_default()
}

fn write_rolling_summary(root: &Path, summary: &str, through: &str) -> Result<()> {
    let path = root.join("summary.md");
    let temporary = path.with_extension("md.tmp");
    fs::write(
        &temporary,
        format!(
            "# Rolling activity summary (through {through})\n\n{}\n",
            summary.trim()
        ),
    )?;
    replace_file(&temporary, &path)
}

fn replace_file(temporary: &Path, target: &Path) -> Result<()> {
    for attempt in 0..5 {
        if target.exists() {
            let _ = fs::remove_file(target);
        }
        match fs::rename(temporary, target) {
            Ok(()) => return Ok(()),
            Err(_error) if attempt < 4 => {
                std::thread::sleep(Duration::from_millis(100 * (attempt + 1)));
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("file replacement retry loop always returns")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn purge_expired_frames(config: &AppConfig) -> Result<()> {
    let pending_paths = fs::read_dir(config.activity_artifacts_dir.join("pending"))?
        .flatten()
        .filter_map(|entry| fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<Block>(&bytes).ok())
        .flat_map(|block| block.frames.into_iter().map(|frame| frame.path))
        .collect::<HashSet<_>>();
    let retention = Duration::from_secs(config.activity_frame_retention_minutes * 60);
    for day in fs::read_dir(&config.activity_artifacts_dir)?.flatten() {
        let frames = day.path().join("frames");
        let Ok(entries) = fs::read_dir(frames) else {
            continue;
        };
        for entry in entries.flatten() {
            if pending_paths.contains(&entry.path().display().to_string()) {
                continue;
            }
            let old_enough = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .and_then(|modified| {
                    SystemTime::now()
                        .duration_since(modified)
                        .map_err(std::io::Error::other)
                })
                .is_ok_and(|age| age >= retention);
            if old_enough {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

fn set_status(
    status: &Arc<Mutex<ActivityStatus>>,
    running: bool,
    summary: String,
    current_frames: usize,
) {
    *status.lock().unwrap_or_else(|error| error.into_inner()) = ActivityStatus {
        running,
        summary,
        current_frames,
    };
}

fn set_work_status(status: &Arc<Mutex<ActivityStatus>>, summary: String, current_frames: usize) {
    let mut current = status.lock().unwrap_or_else(|error| error.into_inner());
    current.summary = summary;
    current.current_frames = current_frames;
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn day_key(ts: i64) -> String {
    clock(ts, "%Y-%m-%d")
}

fn clock(ts: i64, format: &str) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|stamp| stamp.format(format).to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod unit {
    use super::{
        ActivitySample, Block, BlockArtifact, DedupState, FrameRecord, capture_interval_for_sample,
        context_detailed_offset, json_journal_entry, matches_denylist, select_frame_candidates,
        select_frame_candidates_with_size, should_suppress_description,
    };
    use crate::block_artifact::{ActivitySubject, BLOCK_SCHEMA_VERSION};
    use std::collections::BTreeMap;

    #[test]
    fn context_keeps_only_the_configured_number_of_recent_blocks_in_full() {
        assert_eq!(context_detailed_offset(3, 6), 0);
        assert_eq!(context_detailed_offset(7, 6), 1);
        assert_eq!(context_detailed_offset(7, 0), 7);
    }

    #[test]
    fn journal_entry_is_aggregated_from_the_report_field() {
        let artifact = BlockArtifact {
            schema_version: BLOCK_SCHEMA_VERSION,
            block: "1970-01-01T0000".to_string(),
            window_start: 0,
            window_end: 600,
            outcome: "described".to_string(),
            active_seconds: 600,
            idle_seconds: 0,
            app_seconds: BTreeMap::new(),
            timeline: Vec::new(),
            frames_captured: 1,
            frames_sent: 1,
            keyframe: None,
            model: Some("model".to_string()),
            title: Some("Title".to_string()),
            report: Some("Report content only.".to_string()),
            subjects: vec![ActivitySubject {
                namespaces: vec!["work".to_string()],
                subject: "Structured subject must not be rendered.".to_string(),
                estimated_duration_s: 600,
                unattended: false,
                software_development: None,
            }],
            learning_subjects: Vec::new(),
            reason: None,
            error: None,
        };

        let (_, entry) = json_journal_entry(&artifact).unwrap();
        assert!(entry.contains("Report content only."));
        assert!(!entry.contains("Structured subject must not be rendered."));
    }

    #[test]
    fn unified_capture_keeps_ten_second_cadence_while_unlocked() {
        let mut sample = ActivitySample::default();
        assert_eq!(capture_interval_for_sample(&sample, 10), Some(10));

        sample.idle_s = 120.0;
        assert_eq!(capture_interval_for_sample(&sample, 10), Some(10));

        sample.locked = true;
        assert_eq!(capture_interval_for_sample(&sample, 10), None);
    }

    #[test]
    fn dense_capture_denylist_matches_foreground_executable_and_title() {
        let sample = ActivitySample {
            exe: "PasswordManager.exe".to_string(),
            title: "Personal vault".to_string(),
            ..Default::default()
        };
        assert!(matches_denylist(&["passwordmanager".to_string()], &sample));
        assert!(matches_denylist(&["personal vault".to_string()], &sample));
        assert!(!matches_denylist(&["unrelated".to_string()], &sample));
    }

    #[test]
    fn unified_selection_retains_static_evidence_at_the_configured_gap() {
        let frames = (0..=6)
            .map(|index| FrameRecord {
                path: format!("frame-{index}"),
                ts: index * 10,
                monitor: 1,
                width: 100,
                height: 100,
                delta: 0.0,
                window: "document".to_string(),
                sent_to_model: false,
            })
            .collect::<Vec<_>>();
        let selected = select_frame_candidates(frames.iter().collect(), 30, 2.0, 100, 48.0);
        assert_eq!(
            selected.iter().map(|frame| frame.ts).collect::<Vec<_>>(),
            vec![0, 30, 60]
        );
    }

    #[test]
    fn frame_selection_enforces_independent_count_and_payload_limits() {
        let frames = (0..=5)
            .map(|index| FrameRecord {
                path: format!("frame-{index}"),
                ts: index * 10,
                monitor: 1,
                width: 100,
                height: 100,
                delta: index as f32 + 1.0,
                window: "document".to_string(),
                sent_to_model: false,
            })
            .collect::<Vec<_>>();

        let count_limited =
            select_frame_candidates_with_size(frames.iter().collect(), 120, 0.0, 3, 48.0, |_| {
                Some(100)
            });
        assert_eq!(count_limited.len(), 3);
        assert_eq!(count_limited.first().unwrap().ts, 0);
        assert_eq!(count_limited.last().unwrap().ts, 50);

        let payload_limited =
            select_frame_candidates_with_size(frames.iter().collect(), 120, 0.0, 100, 1.0, |_| {
                Some(250_000)
            });
        assert_eq!(payload_limited.len(), 2);
        assert_eq!(payload_limited.first().unwrap().ts, 0);
        assert_eq!(payload_limited.last().unwrap().ts, 50);
    }

    #[test]
    fn pending_manifest_round_trips_the_unified_pipeline_shape() {
        let block = Block::new(0, 600);
        let recovered: Block =
            serde_json::from_slice(&serde_json::to_vec(&block).unwrap()).unwrap();
        assert_eq!(
            recovered.schema_version,
            super::PENDING_BLOCK_SCHEMA_VERSION
        );
        assert_eq!(recovered.attempts, 0);
    }

    #[test]
    fn description_is_suppressed_only_for_low_activity_and_static_visuals() {
        assert!(should_suppress_description(29, 1, 30));
        assert!(!should_suppress_description(30, 1, 30));
        assert!(!should_suppress_description(0, 2, 30));
        assert!(!should_suppress_description(0, 1, 0));
    }

    #[test]
    fn dedup_compares_against_the_last_retained_fingerprint() {
        let mut state = DedupState::default();
        assert_eq!(state.observe(vec![0; 100], 0, 0.5, 120), 100.0);

        let mut gradual = vec![0; 100];
        gradual[0] = 5;
        assert_eq!(state.observe(gradual, 20, 0.5, 120), 0.0);

        let mut accumulated = vec![0; 100];
        accumulated[0] = 10;
        assert_eq!(state.observe(accumulated, 40, 0.5, 120), 1.0);
        assert_eq!(state.retained_ts, Some(40));
    }

    #[test]
    fn maximum_gap_advances_the_visual_baseline() {
        let mut state = DedupState::default();
        state.observe(vec![0; 100], 0, 2.0, 120);

        let mut low_delta = vec![0; 100];
        low_delta[0] = 10;
        assert_eq!(state.observe(low_delta.clone(), 120, 2.0, 120), 1.0);
        assert_eq!(state.retained_ts, Some(120));
        assert_eq!(state.observe(low_delta, 140, 2.0, 120), 0.0);
    }
}

#[cfg(test)]
mod component {
    use super::{Block, BlockArtifact, DescriptionQueue, write_report};
    use crate::block_artifact::{
        ActivityNarrative, ActivitySubject, LearningDepth, LearningMode, LearningRecord,
        LearningSubject,
    };
    use crate::config::AppConfig;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn delayed_description_processing_does_not_block_the_capture_caller() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let queue = DescriptionQueue::spawn_with_processor(move |_block| {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        });
        let block = Block::new(0, 600);
        let before = Instant::now();
        queue.enqueue(block).unwrap();
        assert!(before.elapsed() < Duration::from_millis(100));
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        release_tx.send(()).unwrap();
    }

    #[test]
    fn activity_and_learning_are_written_to_one_block_artifact() {
        let root = tempfile_directory("unified-learning");
        let mut config = AppConfig::synthetic(root.clone());
        config.activity_artifacts_dir = root.clone();
        config.journal_model = "test-model".to_string();
        let block = Block::new(0, 600);
        let narrative = ActivityNarrative {
            title: "Reviewed Rust ownership".to_string(),
            report: "The user reviewed ownership material.".to_string(),
            subjects: vec![ActivitySubject {
                namespaces: vec!["learning".to_string(), "reading".to_string()],
                subject: "Reviewed ownership material.".to_string(),
                estimated_duration_s: 300,
                unattended: false,
                software_development: None,
            }],
            learning_subjects: vec![LearningSubject {
                tags: vec!["rust".to_string(), "ownership".to_string()],
                subject: "Reviewed Rust ownership rules.".to_string(),
                estimated_duration_s: 240,
                learning: LearningRecord {
                    modes: vec![LearningMode::Reading],
                    search_queries: Vec::new(),
                    sources: Vec::new(),
                    depth: LearningDepth::FocusedExplanation,
                },
            }],
        };
        let metrics = super::BlockMetrics {
            timeline: vec!["00:00:00-00:10:00 material".to_string()],
            totals: "browser: 10m00s".to_string(),
            app_seconds: BTreeMap::from([("browser".to_string(), 600)]),
            active_seconds: 300,
            idle_seconds: 300,
        };

        write_report(&config, &block, &narrative, &metrics, 4).unwrap();

        let artifact: BlockArtifact =
            serde_json::from_slice(&fs::read(super::report_path(&root, &block)).unwrap()).unwrap();
        assert_eq!(artifact.subjects, narrative.subjects);
        assert_eq!(artifact.learning_subjects, narrative.learning_subjects);
        assert_eq!(artifact.frames_sent, 4);
        assert_eq!(artifact.model.as_deref(), Some("test-model"));
        assert!(!root.join("1970-01-01").join("learning").exists());
        assert_eq!(
            super::load_successful_reports(&root, i64::MAX)
                .unwrap()
                .len(),
            1
        );

        fs::remove_dir_all(root).unwrap();
    }

    fn tempfile_directory(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ashe-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
}

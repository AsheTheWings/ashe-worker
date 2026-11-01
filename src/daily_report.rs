use crate::block_artifact::{BlockArtifact, BlockDocumentInput, LearningDepth, LearningMode};
use crate::config::AppConfig;
use crate::logger;
use anyhow::Result;
use chrono::{Local, NaiveDate, TimeZone};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAILY_SCAN_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DAILY_GENERATOR_VERSION: &str = "ashe-worker-daily-v7";

pub struct DailyReportHandle {
    tx: Option<Sender<()>>,
}

impl DailyReportHandle {
    pub fn spawn(config: AppConfig) -> Self {
        if !config.daily_report_enabled {
            return Self { tx: None };
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || run(config, rx));
        Self { tx: Some(tx) }
    }
}

impl Drop for DailyReportHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct DailyTotals {
    active_seconds: u64,
    idle_seconds: u64,
    app_seconds: BTreeMap<String, u64>,
    outcomes: BTreeMap<String, usize>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct NamespaceStats {
    estimated_duration_s: u64,
    subjects: usize,
    unattended_subjects: usize,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct CoverageGap {
    start: String,
    end: String,
    status: &'static str,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct DailyCoverage {
    complete: bool,
    gaps: Vec<CoverageGap>,
}

struct DailySource {
    blocks: Vec<BlockDocumentInput>,
    pending_ids: HashSet<String>,
    totals: DailyTotals,
    namespace_stats: BTreeMap<String, NamespaceStats>,
    coverage: DailyCoverage,
    hash: String,
}

fn run(config: AppConfig, stop: Receiver<()>) {
    loop {
        if let Err(error) = scan_closed_days(&config) {
            logger::info(format!("Daily report scan failed: {error:#}"));
        }
        match stop.recv_timeout(DAILY_SCAN_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn scan_closed_days(config: &AppConfig) -> Result<()> {
    let root = &config.activity_artifacts_dir;
    let today = Local::now().format("%Y-%m-%d").to_string();
    let mut days = fs::read_dir(root)?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            (name.len() == 10 && name.as_str() < today.as_str()).then_some(name)
        })
        .collect::<Vec<_>>();
    days.sort();
    for day in days {
        if !grace_period_elapsed(&day, config.daily_report_grace_minutes) {
            continue;
        }
        if let Err(error) = generate_day(config, &day) {
            logger::info(format!("Daily report for {day} failed: {error:#}"));
        }
    }
    Ok(())
}

pub(crate) fn grace_period_elapsed(day: &str, grace_minutes: u64) -> bool {
    day_bounds(day).is_some_and(|(_, end)| now() >= end.saturating_add((grace_minutes * 60) as i64))
}

pub(crate) fn generate_day(config: &AppConfig, day: &str) -> Result<()> {
    let source = build_daily_source(config, day)?;
    let output = config.activity_artifacts_dir.join(day).join("daily.md");
    if read_frontmatter_value(&output, "source_hash").as_deref() == Some(&source.hash) {
        return Ok(());
    }

    let timezone = Local::now().offset().to_string();
    let body = render_daily_report(&source);
    let document = format!(
        "---\ndate: {day}\ntimezone: {timezone}\ngenerated_at: {}\nsource_hash: {}\ngenerator_version: {}\nsource_blocks: {}\npending_blocks: {}\ncoverage_complete: {}\n---\n\n# Activity report — {day}\n\n{}",
        Local::now().to_rfc3339(),
        source.hash,
        DAILY_GENERATOR_VERSION,
        source.blocks.len(),
        source.pending_ids.len(),
        source.coverage.complete,
        body,
    );
    let _write_guard = crate::artifact_store::lock(&config.activity_artifacts_dir)?;
    // An uploaded day removed meanwhile gets no new report.
    if !output.parent().is_some_and(Path::is_dir) {
        return Ok(());
    }
    write_atomic(&output, document.as_bytes())?;
    logger::info(format!(
        "Daily report written: {} ({} blocks, complete={})",
        output.display(),
        source.blocks.len(),
        source.coverage.complete
    ));
    Ok(())
}

pub(crate) fn report_is_current(config: &AppConfig, day: &str) -> bool {
    let output = config.activity_artifacts_dir.join(day).join("daily.md");
    build_daily_source(config, day).is_ok_and(|source| {
        read_frontmatter_value(&output, "source_hash").as_deref() == Some(&source.hash)
    })
}

fn build_daily_source(config: &AppConfig, day: &str) -> Result<DailySource> {
    let mut blocks = load_day_reports(config, day)?;
    blocks.sort_by(|left, right| left.block.cmp(&right.block));
    let pending_ids = load_pending_ids(&config.activity_artifacts_dir, day);
    let report_ids = blocks
        .iter()
        .map(|block| block.block.clone())
        .collect::<HashSet<_>>();
    let coverage = coverage_input(
        day,
        config.activity_block_minutes,
        &report_ids,
        &pending_ids,
    );
    let totals = measured_totals(&blocks);
    let namespace_stats = namespace_totals(&blocks);
    let mut pending_for_hash = pending_ids.iter().cloned().collect::<Vec<_>>();
    pending_for_hash.sort();
    let projected_blocks = blocks
        .iter()
        .map(|block| {
            json!({
                "block": block.block,
                "window_start": block.window_start,
                "window_end": block.window_end,
                "outcome": block.outcome,
                "active_seconds": block.active_seconds,
                "idle_seconds": block.idle_seconds,
                "app_seconds": block.app_seconds,
                "timeline": block.timeline,
                "title": block.title,
                "subjects": block.subjects,
                "learning_subjects": block.learning_subjects,
            })
        })
        .collect::<Vec<_>>();
    let hash_source = json!({
        "generator_version": DAILY_GENERATOR_VERSION,
        "day": day,
        "block_minutes": config.activity_block_minutes,
        "pending_block_ids": pending_for_hash,
        "measured_totals": &totals,
        "namespace_stats": &namespace_stats,
        "coverage": &coverage,
        "blocks": projected_blocks,
    });
    Ok(DailySource {
        blocks,
        pending_ids,
        totals,
        namespace_stats,
        coverage,
        hash: stable_hash(&serde_json::to_vec(&hash_source)?),
    })
}

fn render_daily_report(source: &DailySource) -> String {
    let mut output = String::new();
    output.push_str("## Blocks\n\n");
    if source.blocks.is_empty() {
        output.push_str("No completed blocks.\n\n");
    } else {
        let mut blocks = source.blocks.iter().collect::<Vec<_>>();
        blocks.sort_by(|left, right| left.block.cmp(&right.block));
        for block in blocks {
            let title = block.title.as_deref().unwrap_or(&block.outcome);
            output.push_str(&format!(
                "### {}–{} — {}\n\n",
                local_clock(block.window_start, "%H:%M"),
                local_clock(block.window_end, "%H:%M"),
                markdown_inline(title),
            ));
            output.push_str(&format!(
                "- Telemetry: {} active, {} idle; outcome `{}`.\n",
                human_duration(block.active_seconds),
                human_duration(block.idle_seconds),
                markdown_code(&block.outcome),
            ));
            if block.subjects.is_empty() {
                output.push_str("- Namespaces: none.\n");
            } else {
                output.push_str("- Subjects:\n");
                for subject in &block.subjects {
                    let namespace = namespace_path(&subject.namespaces);
                    let attention = if subject.unattended {
                        ", unattended"
                    } else {
                        ""
                    };
                    let development =
                        subject
                            .software_development
                            .as_ref()
                            .map_or_else(String::new, |record| {
                                let project =
                                    record.project.as_deref().unwrap_or("unknown project");
                                let execution = if record.agentic { "agentic" } else { "manual" };
                                let environment = record
                                    .medium
                                    .as_ref()
                                    .zip(record.tool.as_ref())
                                    .map_or_else(String::new, |(medium, tool)| {
                                        format!(
                                            ", medium `{}`, tool `{}`",
                                            markdown_code(medium),
                                            markdown_code(tool)
                                        )
                                    });
                                let model = record
                                    .model_id
                                    .as_ref()
                                    .map_or_else(String::new, |model_id| {
                                        format!(", model `{}`", markdown_code(model_id))
                                    });
                                format!(
                                    ", project `{}`, {execution}{environment}{model}",
                                    markdown_code(project)
                                )
                            });
                    output.push_str(&format!(
                        "  - `{}` — {} ({}{}{})\n",
                        markdown_code(&namespace),
                        markdown_inline(&subject.subject),
                        human_duration(subject.estimated_duration_s),
                        attention,
                        development,
                    ));
                }
            }
            if block.learning_subjects.is_empty() {
                output.push_str("- Validated learning: none.\n");
            } else {
                output.push_str("- Validated learning:\n");
                for subject in &block.learning_subjects {
                    let modes = subject
                        .learning
                        .modes
                        .iter()
                        .map(learning_mode)
                        .collect::<Vec<_>>()
                        .join(", ");
                    output.push_str(&format!(
                        "  - `{}` — {} ({}; modes `{}`; depth `{}`)\n",
                        markdown_code(&subject.tags.join(", ")),
                        markdown_inline(&subject.subject),
                        human_duration(subject.estimated_duration_s),
                        markdown_code(&modes),
                        markdown_code(learning_depth(&subject.learning.depth)),
                    ));
                }
            }
            if block.timeline.is_empty() {
                output.push_str("- Timeline: unavailable.\n\n");
            } else {
                output.push_str("- Timeline:\n");
                for line in &block.timeline {
                    output.push_str(&format!("  - {}\n", markdown_inline(line)));
                }
                output.push('\n');
            }
        }
    }

    output.push_str("## Telemetry statistics\n\n");
    output.push_str(&format!(
        "- Active: {}\n- Idle: {}\n",
        human_duration(source.totals.active_seconds),
        human_duration(source.totals.idle_seconds),
    ));
    output.push_str("\n### Applications\n\n");
    if source.totals.app_seconds.is_empty() {
        output.push_str("No measured application time.\n");
    } else {
        let mut apps = source.totals.app_seconds.iter().collect::<Vec<_>>();
        apps.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
        output.push_str("| Application | Measured time |\n| --- | ---: |\n");
        for (app, seconds) in apps {
            output.push_str(&format!(
                "| {} | {} |\n",
                markdown_table(app),
                human_duration(*seconds)
            ));
        }
    }
    output.push_str("\n### Outcomes\n\n");
    if source.totals.outcomes.is_empty() {
        output.push_str("No block outcomes.\n");
    } else {
        output.push_str("| Outcome | Blocks |\n| --- | ---: |\n");
        for (outcome, count) in &source.totals.outcomes {
            output.push_str(&format!("| `{}` | {} |\n", markdown_code(outcome), count));
        }
    }

    output.push_str("\n## Namespace statistics\n\n");
    output.push_str(
        "Durations are independent model estimates and may overlap across subjects and namespace prefixes.\n\n",
    );
    if source.namespace_stats.is_empty() {
        output.push_str("No subject namespaces.\n");
    } else {
        output.push_str(
            "| Namespace | Estimated time | Subjects | Unattended |\n| --- | ---: | ---: | ---: |\n",
        );
        for (namespace, stats) in &source.namespace_stats {
            output.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                markdown_code(namespace),
                human_duration(stats.estimated_duration_s),
                stats.subjects,
                stats.unattended_subjects,
            ));
        }
    }

    output.push_str("\n## Coverage gaps\n\n");
    if source.coverage.complete {
        output.push_str("Complete coverage.\n");
    } else {
        for gap in &source.coverage.gaps {
            output.push_str(&format!(
                "- {}–{}: {}.\n",
                markdown_inline(&gap.start),
                markdown_inline(&gap.end),
                markdown_inline(gap.status),
            ));
        }
    }
    output
}

fn load_day_reports(config: &AppConfig, day: &str) -> Result<Vec<BlockDocumentInput>> {
    let directory = config.activity_artifacts_dir.join(day).join("blocks");
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(Vec::new());
    };
    let mut reports = BTreeMap::new();
    for path in entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
    {
        let Ok(bytes) = fs::read(path) else { continue };
        let Ok(artifact) = serde_json::from_slice::<BlockArtifact>(&bytes) else {
            continue;
        };
        if !artifact.has_current_schema() {
            continue;
        }
        reports.insert(artifact.block.clone(), artifact.document_input());
    }
    Ok(reports.into_values().collect())
}

pub(crate) fn has_pending_blocks(root: &Path, day: &str) -> bool {
    !load_pending_ids(root, day).is_empty()
}

fn load_pending_ids(root: &Path, day: &str) -> HashSet<String> {
    let Ok(entries) = fs::read_dir(root.join("pending")) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|value| value.get("day").and_then(Value::as_str) == Some(day))
        .filter_map(|value| value.get("start").and_then(Value::as_i64))
        .map(|start| format!("{}T{}", day, local_clock(start, "%H%M")))
        .collect()
}

fn coverage_input(
    day: &str,
    block_minutes: u64,
    reports: &HashSet<String>,
    pending: &HashSet<String>,
) -> DailyCoverage {
    let Some((start, end)) = day_bounds(day) else {
        return DailyCoverage {
            complete: false,
            gaps: vec![CoverageGap {
                start: day.to_string(),
                end: day.to_string(),
                status: "invalid local date",
            }],
        };
    };
    let span = (block_minutes * 60) as i64;
    let mut cursor = start;
    let remainder = cursor.rem_euclid(span);
    if remainder != 0 {
        cursor += span - remainder;
    }
    let mut gaps: Vec<(i64, i64, &'static str)> = Vec::new();
    if cursor > start {
        gaps.push((start, cursor.min(end), "missing/unknown"));
    }
    while cursor < end {
        let block_end = (cursor + span).min(end);
        let id = format!("{}T{}", day, local_clock(cursor, "%H%M"));
        let status = if reports.contains(&id) {
            None
        } else if pending.contains(&id) {
            Some("pending description")
        } else {
            Some("missing/unknown")
        };
        if let Some(status) = status {
            if let Some(last) = gaps.last_mut()
                && last.1 == cursor
                && last.2 == status
            {
                last.1 = block_end;
            } else {
                gaps.push((cursor, block_end, status));
            }
        }
        cursor += span;
    }
    let gaps = gaps
        .into_iter()
        .map(|(start, end, status)| CoverageGap {
            start: local_clock(start, "%H:%M"),
            end: local_clock(end, "%H:%M"),
            status,
        })
        .collect::<Vec<_>>();
    DailyCoverage {
        complete: gaps.is_empty(),
        gaps,
    }
}

fn measured_totals(blocks: &[BlockDocumentInput]) -> DailyTotals {
    let active_seconds = blocks.iter().map(|block| block.active_seconds).sum();
    let idle_seconds = blocks.iter().map(|block| block.idle_seconds).sum();
    let mut app_seconds = BTreeMap::new();
    let mut outcomes = BTreeMap::new();
    for block in blocks {
        *outcomes.entry(block.outcome.clone()).or_default() += 1;
        for (app, seconds) in &block.app_seconds {
            *app_seconds.entry(app.clone()).or_default() += seconds;
        }
    }
    DailyTotals {
        active_seconds,
        idle_seconds,
        app_seconds,
        outcomes,
    }
}

fn namespace_totals(blocks: &[BlockDocumentInput]) -> BTreeMap<String, NamespaceStats> {
    let mut totals = BTreeMap::new();
    for subject in blocks.iter().flat_map(|block| &block.subjects) {
        if subject.namespaces.is_empty() {
            add_namespace_stat(&mut totals, "unclassified", subject);
            continue;
        }
        for length in 1..=subject.namespaces.len() {
            let namespace = namespace_path(&subject.namespaces[..length]);
            add_namespace_stat(&mut totals, &namespace, subject);
        }
    }
    totals
}

fn add_namespace_stat(
    totals: &mut BTreeMap<String, NamespaceStats>,
    namespace: &str,
    subject: &crate::block_artifact::ActivitySubject,
) {
    let stats = totals
        .entry(namespace.to_string())
        .or_insert(NamespaceStats {
            estimated_duration_s: 0,
            subjects: 0,
            unattended_subjects: 0,
        });
    stats.estimated_duration_s = stats
        .estimated_duration_s
        .saturating_add(subject.estimated_duration_s);
    stats.subjects += 1;
    if subject.unattended {
        stats.unattended_subjects += 1;
    }
}

fn namespace_path(namespaces: &[String]) -> String {
    if namespaces.is_empty() {
        "unclassified".to_string()
    } else {
        namespaces.join(" / ")
    }
}

fn learning_depth(depth: &LearningDepth) -> &'static str {
    match depth {
        LearningDepth::Lookup => "lookup",
        LearningDepth::Orientation => "orientation",
        LearningDepth::FocusedExplanation => "focused-explanation",
        LearningDepth::Procedural => "procedural",
        LearningDepth::Applied => "applied",
        LearningDepth::Synthesis => "synthesis",
    }
}

fn learning_mode(mode: &LearningMode) -> &'static str {
    match mode {
        LearningMode::Lookup => "lookup",
        LearningMode::Reading => "reading",
        LearningMode::Watching => "watching",
        LearningMode::Coursework => "coursework",
        LearningMode::Practice => "practice",
        LearningMode::Discussion => "discussion",
    }
}

fn human_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn markdown_inline(text: &str) -> String {
    text.replace(['\r', '\n'], " ").trim().to_string()
}

fn markdown_code(text: &str) -> String {
    markdown_inline(text).replace('`', "'")
}

fn markdown_table(text: &str) -> String {
    markdown_inline(text).replace('|', "\\|")
}

fn frontmatter_value(content: &str, name: &str) -> Option<String> {
    if !content.starts_with("---\n") {
        return None;
    }
    let prefix = format!("{name}: ");
    content
        .lines()
        .skip(1)
        .take_while(|line| *line != "---")
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn read_frontmatter_value(path: &Path, name: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| frontmatter_value(&content, name))
}

fn day_bounds(day: &str) -> Option<(i64, i64)> {
    let date = NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let next = date.succ_opt()?;
    let start = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .earliest()?
        .timestamp();
    let end = Local
        .from_local_datetime(&next.and_hms_opt(0, 0, 0)?)
        .earliest()?
        .timestamp();
    Some((start, end))
}

fn local_clock(ts: i64, format: &str) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|stamp| stamp.format(format).to_string())
        .unwrap_or_default()
}

fn stable_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let temporary = PathBuf::from(format!("{}.tmp", path.display()));
    fs::write(&temporary, content)?;
    for attempt in 0..5 {
        if path.exists() {
            let _ = fs::remove_file(path);
        }
        match fs::rename(&temporary, path) {
            Ok(()) => return Ok(()),
            Err(_error) if attempt < 4 => {
                std::thread::sleep(Duration::from_millis(100 * (attempt + 1)));
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("daily report replacement retry loop always returns")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod unit {
    use super::{
        DailyCoverage, DailySource, DailyTotals, NamespaceStats, coverage_input, day_bounds,
        local_clock, namespace_totals, render_daily_report,
    };
    use crate::block_artifact::{
        ActivitySubject, BlockDocumentInput, LearningDepth, LearningMode, LearningRecord,
        LearningSubject, SoftwareDevelopmentRecord,
    };
    use std::collections::{BTreeMap, HashSet};

    fn block(subjects: Vec<ActivitySubject>) -> BlockDocumentInput {
        BlockDocumentInput {
            block: "2026-07-28T1200".to_string(),
            window_start: 1_775_000_000,
            window_end: 1_775_000_600,
            outcome: "described".to_string(),
            active_seconds: 420,
            idle_seconds: 180,
            app_seconds: BTreeMap::from([("code.exe".to_string(), 600)]),
            timeline: vec!["12:00:00-12:10:00 (10m00s) code.exe — Ashe".to_string()],
            title: Some("Implemented learning records".to_string()),
            report: Some("This prose must not drive the daily report.".to_string()),
            subjects,
            learning_subjects: Vec::new(),
            reason: None,
            error: None,
        }
    }

    #[test]
    fn namespace_statistics_include_every_prefix_and_attention_state() {
        let blocks = vec![block(vec![ActivitySubject {
            namespaces: vec![
                "software-development".to_string(),
                "coding".to_string(),
                "learning-records".to_string(),
            ],
            subject: "Implemented the change.".to_string(),
            estimated_duration_s: 300,
            unattended: true,
            software_development: Some(SoftwareDevelopmentRecord {
                project: Some("ashe-worker".to_string()),
                agentic: true,
                medium: Some("terminal".to_string()),
                tool: Some("codex".to_string()),
                model_id: Some("gpt-5.4".to_string()),
            }),
        }])];
        let stats = namespace_totals(&blocks);
        assert_eq!(stats.len(), 3);
        assert_eq!(stats["software-development"].estimated_duration_s, 300);
        assert_eq!(
            stats["software-development / coding"].unattended_subjects,
            1
        );
        assert_eq!(
            stats["software-development / coding / learning-records"].subjects,
            1
        );
    }

    #[test]
    fn coverage_distinguishes_pending_and_missing_intervals() {
        let day = "2026-07-28";
        let (start, _) = day_bounds(day).unwrap();
        let span = 720 * 60;
        let first = start + (span - start.rem_euclid(span)).rem_euclid(span);
        let reports = HashSet::from([format!("{day}T{}", local_clock(first, "%H%M"))]);
        let pending = HashSet::from([format!("{day}T{}", local_clock(first + span, "%H%M"))]);
        let pending_coverage = coverage_input(day, 720, &reports, &pending);
        assert!(!pending_coverage.complete);
        assert!(
            pending_coverage
                .gaps
                .iter()
                .any(|gap| gap.status == "pending description")
        );

        let missing_coverage = coverage_input(day, 720, &reports, &HashSet::new());
        assert!(
            missing_coverage
                .gaps
                .iter()
                .any(|gap| gap.status == "missing/unknown")
        );
    }

    #[test]
    fn daily_markdown_is_a_deterministic_structured_projection() {
        let mut later = block(vec![ActivitySubject {
            namespaces: vec!["learning".to_string(), "lookup".to_string()],
            subject: "Looked up a Rust term.".to_string(),
            estimated_duration_s: 120,
            unattended: false,
            software_development: None,
        }]);
        later.learning_subjects = vec![LearningSubject {
            tags: vec!["rust".to_string(), "ownership".to_string()],
            subject: "Reviewed how Rust ownership transfers values.".to_string(),
            estimated_duration_s: 90,
            learning: LearningRecord {
                modes: vec![LearningMode::Reading],
                search_queries: Vec::new(),
                sources: Vec::new(),
                depth: LearningDepth::FocusedExplanation,
            },
        }];
        let mut earlier = block(vec![]);
        earlier.block = "2026-07-28T1100".to_string();
        earlier.window_start -= 3_600;
        earlier.window_end -= 3_600;
        earlier.title = Some("Earlier title".to_string());
        earlier.timeline = vec!["11:00:00-11:10:00 earlier timeline".to_string()];
        let source = DailySource {
            totals: DailyTotals {
                active_seconds: 420,
                idle_seconds: 180,
                app_seconds: BTreeMap::from([("code.exe".to_string(), 600)]),
                outcomes: BTreeMap::from([("described".to_string(), 1)]),
            },
            namespace_stats: BTreeMap::from([(
                "learning".to_string(),
                NamespaceStats {
                    estimated_duration_s: 120,
                    subjects: 1,
                    unattended_subjects: 0,
                },
            )]),
            coverage: DailyCoverage {
                complete: false,
                gaps: vec![super::CoverageGap {
                    start: "00:00".to_string(),
                    end: "12:00".to_string(),
                    status: "missing/unknown",
                }],
            },
            pending_ids: HashSet::new(),
            blocks: vec![later, earlier],
            hash: "hash".to_string(),
        };
        let report = render_daily_report(&source);
        assert!(report.contains("## Blocks"));
        assert!(
            report.find("Earlier title").unwrap() < report.find("Implemented learning").unwrap()
        );
        assert!(report.contains("earlier timeline"));
        assert!(report.contains("`learning / lookup`"));
        assert!(report.contains("Reviewed how Rust ownership transfers values."));
        assert!(report.contains("modes `reading`"));
        assert!(report.contains("depth `focused-explanation`"));
        assert!(report.contains("## Telemetry statistics"));
        assert!(report.contains("## Namespace statistics"));
        assert!(report.contains("## Coverage gaps"));
        assert!(report.contains("00:00–12:00: missing/unknown"));
        assert!(!report.contains("This prose must not drive the daily report."));
    }
}

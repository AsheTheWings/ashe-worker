//! Typed configuration built from the validated inputs.

use crate::inputs::{self, Environment, InputError, Sources};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exporter {
    Otlp,
    Console,
    None,
}

impl Exporter {
    fn parse(value: &str) -> Self {
        match value {
            "otlp" => Self::Otlp,
            "console" => Self::Console,
            _ => Self::None,
        }
    }
}

#[derive(Clone)]
pub struct TelemetryConfig {
    pub deployment_environment: String,
    pub traces: Exporter,
    pub metrics: Exporter,
    pub logs: Exporter,
    pub endpoint: String,
    pub headers: String,
}

#[derive(Clone)]
pub struct AppConfig {
    pub llm_base_url: String,
    pub llm_api_key: String,
    pub grammar_model: String,
    pub question_model: String,
    pub journal_model: String,
    pub llm_temperature: f32,
    pub llm_reasoning_effort: Option<String>,
    pub activity_enabled: bool,
    pub activity_artifacts_dir: PathBuf,
    pub activity_capture_interval: u64,
    pub activity_telemetry_interval_ms: u64,
    pub activity_block_minutes: u64,
    pub activity_monitor: i32,
    pub activity_dedup_threshold: f32,
    pub activity_max_frame_gap_s: u64,
    pub activity_max_frames_per_call: usize,
    pub activity_max_payload_mb: f32,
    pub activity_idle_threshold_s: f64,
    pub activity_min_active_seconds: u64,
    pub activity_denylist: Vec<String>,
    pub activity_frame_retention_minutes: u64,
    pub activity_context_blocks: usize,
    pub activity_context_summary_max_chars: usize,
    pub daily_report_enabled: bool,
    pub daily_report_grace_minutes: u64,
    pub server_url: String,
    pub server_token: String,
    pub upload_scan_minutes: u64,
    pub local_retention_days: u64,
    pub telemetry: TelemetryConfig,
}

impl AppConfig {
    /// Load, layer, and validate this process's inputs.
    pub fn load() -> Result<Self, InputError> {
        let process = crate::system::process_environment();
        let environment = inputs::load(&Sources {
            process: &process,
            known_folder: &crate::system::known_folder,
            read_overlay: &crate::system::read_overlay,
            read_protected: &crate::system::read_protected,
        })?;
        Ok(Self::from_environment(&environment))
    }

    pub fn from_environment(environment: &Environment) -> Self {
        let integer = |name| environment.integer(name);
        let capture_interval = integer("ASHE_WORKER_CAPTURE_INTERVAL_SECONDS") as u64;
        let reasoning = environment.text("ASHE_WORKER_LLM_REASONING_EFFORT");
        Self {
            llm_base_url: environment
                .text("ASHE_WORKER_LLM_BASE_URL")
                .trim_end_matches('/')
                .to_string(),
            llm_api_key: environment.text("ASHE_WORKER_LLM_API_KEY").to_string(),
            grammar_model: environment.text("ASHE_WORKER_GRAMMAR_MODEL").to_string(),
            question_model: environment.text("ASHE_WORKER_QUESTION_MODEL").to_string(),
            journal_model: environment.text("ASHE_WORKER_JOURNAL_MODEL").to_string(),
            llm_temperature: environment.number("ASHE_WORKER_LLM_TEMPERATURE") as f32,
            llm_reasoning_effort: (!reasoning.is_empty()).then(|| reasoning.to_string()),
            activity_enabled: environment.bool("ASHE_WORKER_ACTIVITY_ENABLED"),
            activity_artifacts_dir: environment.path("ASHE_WORKER_DATA_DIR"),
            activity_capture_interval: capture_interval,
            activity_telemetry_interval_ms: (environment
                .number("ASHE_WORKER_SAMPLE_INTERVAL_SECONDS")
                * 1000.0) as u64,
            activity_block_minutes: integer("ASHE_WORKER_BLOCK_MINUTES") as u64,
            activity_monitor: integer("ASHE_WORKER_MONITOR") as i32,
            activity_dedup_threshold: environment.number("ASHE_WORKER_DEDUP_THRESHOLD") as f32,
            activity_max_frame_gap_s: (integer("ASHE_WORKER_MAX_FRAME_GAP_SECONDS") as u64)
                .max(capture_interval),
            activity_max_frames_per_call: integer("ASHE_WORKER_MAX_FRAMES_PER_CALL") as usize,
            activity_max_payload_mb: environment.number("ASHE_WORKER_MAX_PAYLOAD_MB") as f32,
            activity_idle_threshold_s: environment.number("ASHE_WORKER_IDLE_THRESHOLD_SECONDS"),
            activity_min_active_seconds: integer("ASHE_WORKER_MIN_ACTIVE_SECONDS") as u64,
            activity_denylist: environment
                .list("ASHE_WORKER_DENYLIST")
                .into_iter()
                .map(|entry| entry.to_ascii_lowercase())
                .collect(),
            activity_frame_retention_minutes: integer("ASHE_WORKER_FRAME_RETENTION_MINUTES") as u64,
            activity_context_blocks: integer("ASHE_WORKER_CONTEXT_BLOCKS") as usize,
            activity_context_summary_max_chars: integer("ASHE_WORKER_MAX_SUMMARY_CHARS") as usize,
            daily_report_enabled: environment.bool("ASHE_WORKER_DAILY_REPORT_ENABLED"),
            daily_report_grace_minutes: integer("ASHE_WORKER_DAILY_REPORT_GRACE_MINUTES") as u64,
            server_url: environment
                .text("ASHE_WORKER_SERVER_URL")
                .trim_end_matches('/')
                .to_string(),
            server_token: environment.text("ASHE_WORKER_SERVER_TOKEN").to_string(),
            upload_scan_minutes: integer("ASHE_WORKER_UPLOAD_SCAN_MINUTES") as u64,
            local_retention_days: integer("ASHE_WORKER_LOCAL_RETENTION_DAYS") as u64,
            telemetry: TelemetryConfig {
                deployment_environment: environment
                    .text("ASHE_WORKER_DEPLOYMENT_ENVIRONMENT")
                    .to_string(),
                traces: Exporter::parse(environment.text("OTEL_TRACES_EXPORTER")),
                metrics: Exporter::parse(environment.text("OTEL_METRICS_EXPORTER")),
                logs: Exporter::parse(environment.text("OTEL_LOGS_EXPORTER")),
                endpoint: environment
                    .text("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .trim_end_matches('/')
                    .to_string(),
                headers: environment.text("OTEL_EXPORTER_OTLP_HEADERS").to_string(),
            },
        }
    }

    /// A complete configuration with synthetic credentials, rooted at
    /// `data_dir`, for tests.
    #[doc(hidden)]
    pub fn synthetic(data_dir: PathBuf) -> Self {
        Self {
            llm_base_url: "http://127.0.0.1:9/v1".to_string(),
            llm_api_key: "synthetic-llm-key".to_string(),
            grammar_model: "synthetic-grammar".to_string(),
            question_model: "synthetic-question".to_string(),
            journal_model: "synthetic-journal".to_string(),
            llm_temperature: 0.2,
            llm_reasoning_effort: None,
            activity_enabled: true,
            activity_artifacts_dir: data_dir,
            activity_capture_interval: 10,
            activity_telemetry_interval_ms: 2_000,
            activity_block_minutes: 10,
            activity_monitor: 1,
            activity_dedup_threshold: 2.0,
            activity_max_frame_gap_s: 30,
            activity_max_frames_per_call: 100,
            activity_max_payload_mb: 48.0,
            activity_idle_threshold_s: 120.0,
            activity_min_active_seconds: 30,
            activity_denylist: Vec::new(),
            activity_frame_retention_minutes: 30,
            activity_context_blocks: 6,
            activity_context_summary_max_chars: 1_500,
            daily_report_enabled: true,
            daily_report_grace_minutes: 15,
            server_url: "http://127.0.0.1:9".to_string(),
            server_token: "synthetic-server-token".to_string(),
            upload_scan_minutes: 5,
            local_retention_days: 7,
            telemetry: TelemetryConfig {
                deployment_environment: "development".to_string(),
                traces: Exporter::None,
                metrics: Exporter::None,
                logs: Exporter::None,
                endpoint: "http://127.0.0.1:4318".to_string(),
                headers: String::new(),
            },
        }
    }

    /// Configuration facts safe to show the user: no credential values.
    pub fn summary(&self) -> String {
        format!(
            "Models: grammar={} question={} journal={}\r\nActivity tracking: {}\r\nData: {}\r\nServer: {}\r\nEnvironment: {}",
            self.grammar_model,
            self.question_model,
            self.journal_model,
            if self.activity_enabled { "on" } else { "off" },
            self.activity_artifacts_dir.display(),
            self.server_url,
            self.telemetry.deployment_environment,
        )
    }
}

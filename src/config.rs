use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

const DEFAULT_OUTPUT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_TERA_API_BASE: &str = "https://tera.asheservices.online/v1";
const DEFAULT_LLM_MODEL: &str = "gemini-latest";
const DEFAULT_LLM_TEMPERATURE: f32 = 0.2;
const DEFAULT_ACTIVITY_CAPTURE_INTERVAL: u64 = 10;
const DEFAULT_ACTIVITY_MAX_FRAME_GAP_S: u64 = 30;
const DEFAULT_ACTIVITY_BLOCK_MINUTES: u64 = 10;
const DEFAULT_ACTIVITY_DEDUP_THRESHOLD: f32 = 2.0;
const DEFAULT_ACTIVITY_MAX_FRAMES_PER_CALL: usize = 100;
const DEFAULT_ACTIVITY_MIN_ACTIVE_SECONDS: u64 = 30;
const DEFAULT_CONTEXT_BLOCKS: usize = 6;
const DEFAULT_CONTEXT_SUMMARY_MAX_CHARS: usize = 1_500;
const DEFAULT_DAILY_REPORT_GRACE_MINUTES: u64 = 15;
const DEFAULT_ARCHIVE_PLAINTEXT_DAYS: u64 = 2;
const DEFAULT_ARCHIVE_SCAN_MINUTES: u64 = 5;
const COMPILED_ARCHIVE_RECIPIENT_JSON: &str = match option_env!("ASHE_ARCHIVE_RECIPIENT_JSON") {
    Some(value) => value,
    None => "",
};

#[derive(Clone)]
pub struct AppConfig {
    pub fal_api_key: String,
    pub fal_language: String,
    pub stt_backend: String,
    pub stt_token: String,
    pub spoken_punctuation: bool,
    pub output_sample_rate: u32,
    pub tera_api_key: String,
    pub tera_api_base: String,
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
    pub archive_recipient_json: String,
    pub archive_plaintext_days: u64,
    pub archive_scan_minutes: u64,
    pub worker_base_url: String,
    pub archive_upload_token: String,
    pub paste_upload_token: String,
}

impl AppConfig {
    pub fn load() -> Self {
        load_env_file_near_exe();

        let activity_artifacts_dir = std::env::var("ASHE_ARTIFACTS_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_artifacts_dir);
        let activity_capture_interval =
            read_u64("ASHE_CAPTURE_INTERVAL", DEFAULT_ACTIVITY_CAPTURE_INTERVAL).max(1);

        Self {
            fal_api_key: std::env::var("FAL_KEY").unwrap_or_default(),
            fal_language: std::env::var("FAL_LANGUAGE").unwrap_or_else(|_| "eng".to_string()),
            stt_backend: std::env::var("ASHE_STT_BACKEND").unwrap_or_else(|_| "fal".to_string()),
            stt_token: std::env::var("ASHE_STT_TOKEN").unwrap_or_default(),
            spoken_punctuation: read_bool("ASHE_SPOKEN_PUNCTUATION", true),
            output_sample_rate: read_output_sample_rate(),
            tera_api_key: std::env::var("TERA_API_KEY").unwrap_or_default(),
            tera_api_base: std::env::var("TERA_API_BASE")
                .unwrap_or_else(|_| DEFAULT_TERA_API_BASE.to_string()),
            grammar_model: read_llm_model("TERA_GRAMMAR_MODEL"),
            question_model: read_llm_model("TERA_QUESTION_MODEL"),
            journal_model: read_llm_model("TERA_JOURNAL_MODEL"),
            llm_temperature: read_f32("ASHE_LLM_TEMPERATURE", DEFAULT_LLM_TEMPERATURE),
            llm_reasoning_effort: std::env::var("TERA_LLM_REASONING_EFFORT")
                .ok()
                .filter(|value| !value.is_empty()),
            activity_enabled: read_bool("ASHE_ACTIVITY_ENABLED", true),
            activity_artifacts_dir,
            activity_capture_interval,
            activity_telemetry_interval_ms: (read_f32("ASHE_TELEMETRY_INTERVAL", 2.0).max(0.5)
                * 1000.0) as u64,
            activity_block_minutes: read_u64("ASHE_BLOCK_MINUTES", DEFAULT_ACTIVITY_BLOCK_MINUTES)
                .max(1),
            activity_monitor: read_i32("ASHE_MONITOR", 1),
            activity_dedup_threshold: read_f32(
                "ASHE_DEDUP_THRESHOLD",
                DEFAULT_ACTIVITY_DEDUP_THRESHOLD,
            )
            .clamp(0.0, 100.0),
            activity_max_frame_gap_s: read_u64(
                "ASHE_MAX_FRAME_GAP_S",
                DEFAULT_ACTIVITY_MAX_FRAME_GAP_S,
            )
            .max(activity_capture_interval),
            activity_max_frames_per_call: read_usize(
                "ASHE_MAX_FRAMES_PER_CALL",
                DEFAULT_ACTIVITY_MAX_FRAMES_PER_CALL,
            )
            .max(2),
            activity_max_payload_mb: read_f32("ASHE_MAX_PAYLOAD_MB", 48.0).max(1.0),
            activity_idle_threshold_s: read_f32("ASHE_IDLE_THRESHOLD_S", 120.0).max(10.0) as f64,
            activity_min_active_seconds: read_u64(
                "ASHE_MIN_ACTIVE_SECONDS",
                DEFAULT_ACTIVITY_MIN_ACTIVE_SECONDS,
            ),
            activity_denylist: read_list("ASHE_DENYLIST"),
            activity_frame_retention_minutes: read_u64("ASHE_FRAME_RETENTION_MINUTES", 30),
            activity_context_blocks: read_usize("ASHE_CONTEXT_BLOCKS", DEFAULT_CONTEXT_BLOCKS),
            activity_context_summary_max_chars: read_usize(
                "ASHE_MAX_SUMMARY_CHARS",
                DEFAULT_CONTEXT_SUMMARY_MAX_CHARS,
            )
            .max(1),
            daily_report_enabled: read_bool("ASHE_DAILY_REPORT_ENABLED", true),
            daily_report_grace_minutes: read_u64(
                "ASHE_DAILY_REPORT_GRACE_MINUTES",
                DEFAULT_DAILY_REPORT_GRACE_MINUTES,
            ),
            archive_recipient_json: std::env::var("ASHE_ARCHIVE_RECIPIENT_JSON")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| COMPILED_ARCHIVE_RECIPIENT_JSON.to_string()),
            archive_plaintext_days: read_u64(
                "ASHE_ARCHIVE_PLAINTEXT_DAYS",
                DEFAULT_ARCHIVE_PLAINTEXT_DAYS,
            )
            .max(2),
            archive_scan_minutes: read_u64(
                "ASHE_ARCHIVE_SCAN_MINUTES",
                DEFAULT_ARCHIVE_SCAN_MINUTES,
            )
            .max(1),
            worker_base_url: std::env::var("ASHE_WORKER_BASE_URL").unwrap_or_default(),
            archive_upload_token: std::env::var("ASHE_ARCHIVE_UPLOAD_TOKEN").unwrap_or_default(),
            paste_upload_token: std::env::var("ASHE_PASTE_UPLOAD_TOKEN").unwrap_or_default(),
        }
    }

    pub fn validate_for_dictation(&self) -> Result<()> {
        if !(8_000..=192_000).contains(&self.output_sample_rate) {
            return Err(anyhow!(
                "ASHE_OUTPUT_SAMPLE_RATE must be between 8000 and 192000"
            ));
        }
        if self.use_self_hosted_stt() {
            return self.validate_for_self_hosted_stt();
        }
        if self.fal_api_key.trim().is_empty() {
            return Err(anyhow!("FAL_KEY is missing"));
        }
        Ok(())
    }

    pub fn use_self_hosted_stt(&self) -> bool {
        self.stt_backend.trim().eq_ignore_ascii_case("selfhosted")
    }

    pub fn validate_for_self_hosted_stt(&self) -> Result<()> {
        self.worker_endpoint("/v1/stt/transcribe")?;
        if self.stt_token.trim().is_empty() {
            return Err(anyhow!("ASHE_STT_TOKEN is missing"));
        }
        Ok(())
    }

    pub fn validate_for_grammar(&self) -> Result<()> {
        self.validate_llm_model(&self.grammar_model, "TERA_GRAMMAR_MODEL")
    }

    pub fn validate_for_question(&self) -> Result<()> {
        self.validate_llm_model(&self.question_model, "TERA_QUESTION_MODEL")
    }

    pub fn validate_for_journal(&self) -> Result<()> {
        self.validate_llm_model(&self.journal_model, "TERA_JOURNAL_MODEL")
    }

    fn validate_llm_model(&self, model: &str, name: &str) -> Result<()> {
        if self.tera_api_key.trim().is_empty() {
            return Err(anyhow!("TERA_API_KEY is missing"));
        }
        if self.tera_api_base.trim().is_empty() {
            return Err(anyhow!("TERA_API_BASE is empty"));
        }
        if model.trim().is_empty() {
            return Err(anyhow!("{name} is empty"));
        }
        Ok(())
    }

    pub fn worker_endpoint(&self, route: &str) -> Result<String> {
        worker_endpoint_url(&self.worker_base_url, route)
    }

    pub fn validate_for_paste(&self) -> Result<()> {
        self.worker_endpoint("/v1/pastes")?;
        if self.paste_upload_token.trim().is_empty() {
            return Err(anyhow!("ASHE_PASTE_UPLOAD_TOKEN is missing"));
        }
        Ok(())
    }

    pub fn log_summary(&self) -> String {
        format!(
            "language={} stt_backend={} output_sample_rate={} fal_api_key_present={} grammar_model={} question_model={} journal_model={} tera_api_key_present={} reasoning_effort_present={} activity_enabled={} activity_artifacts={} activity_capture_interval={}s activity_max_frame_gap={}s context_blocks={} summary_max_chars={} daily_report_enabled={} daily_grace_minutes={} archive_recipient_present={} archive_plaintext_days={} archive_upload_configured={} paste_upload_configured={}",
            if self.fal_language.trim().is_empty() {
                "auto".to_string()
            } else {
                self.fal_language.clone()
            },
            self.stt_backend.trim(),
            self.output_sample_rate,
            !self.fal_api_key.trim().is_empty(),
            self.grammar_model,
            self.question_model,
            self.journal_model,
            !self.tera_api_key.trim().is_empty(),
            self.llm_reasoning_effort.is_some(),
            self.activity_enabled,
            self.activity_artifacts_dir.display(),
            self.activity_capture_interval,
            self.activity_max_frame_gap_s,
            self.activity_context_blocks,
            self.activity_context_summary_max_chars,
            self.daily_report_enabled,
            self.daily_report_grace_minutes,
            !self.archive_recipient_json.trim().is_empty(),
            self.archive_plaintext_days,
            !self.worker_base_url.trim().is_empty() && !self.archive_upload_token.trim().is_empty(),
            !self.worker_base_url.trim().is_empty() && !self.paste_upload_token.trim().is_empty(),
        )
    }
}

fn read_llm_model(name: &str) -> String {
    resolve_llm_model(std::env::var(name).ok(), std::env::var("TERA_MODEL").ok())
}

fn resolve_llm_model(feature_model: Option<String>, tera_model: Option<String>) -> String {
    feature_model
        .or(tera_model)
        .unwrap_or_else(|| DEFAULT_LLM_MODEL.to_string())
}

fn worker_endpoint_url(base_url: &str, route: &str) -> Result<String> {
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() {
        return Err(anyhow!("ASHE_WORKER_BASE_URL is missing"));
    }
    let parsed = reqwest::Url::parse(base_url)
        .map_err(|_| anyhow!("ASHE_WORKER_BASE_URL is not a valid URL"))?;
    if parsed.scheme() != "https" {
        return Err(anyhow!("ASHE_WORKER_BASE_URL must use HTTPS"));
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        return Err(anyhow!(
            "ASHE_WORKER_BASE_URL must contain only an HTTPS origin"
        ));
    }
    if !route.starts_with('/') {
        return Err(anyhow!("worker endpoint route must start with '/'"));
    }
    Ok(format!("{base_url}{route}"))
}

fn read_output_sample_rate() -> u32 {
    read_u32("ASHE_OUTPUT_SAMPLE_RATE", DEFAULT_OUTPUT_SAMPLE_RATE)
}

fn read_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

fn read_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn read_i32(name: &str, default: i32) -> i32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok())
        .unwrap_or(default)
}

fn read_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

fn read_bool(name: &str, default: bool) -> bool {
    match std::env::var(name)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}

fn read_list(name: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

fn read_f32(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<f32>().ok())
        .unwrap_or(default)
}

fn load_env_file_near_exe() {
    let mut path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    for _ in 0..8 {
        let Some(current) = path.clone() else {
            break;
        };
        let env_path = current.join(".env.local");
        if env_path.exists() {
            let _ = dotenvy::from_path_override(env_path);
            return;
        }
        path = current.parent().map(PathBuf::from);
    }
}

fn default_artifacts_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("artifacts")))
        .unwrap_or_else(|| PathBuf::from("artifacts"))
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_ACTIVITY_CAPTURE_INTERVAL, DEFAULT_ACTIVITY_MAX_FRAME_GAP_S, DEFAULT_LLM_MODEL,
        resolve_llm_model, worker_endpoint_url,
    };

    #[test]
    fn unified_activity_evidence_defaults_to_ten_second_capture() {
        assert_eq!(DEFAULT_ACTIVITY_CAPTURE_INTERVAL, 10);
        assert_eq!(DEFAULT_ACTIVITY_MAX_FRAME_GAP_S, 30);
    }

    #[test]
    fn llm_features_prefer_their_model_and_default_to_gemini_latest() {
        assert_eq!(resolve_llm_model(None, None), DEFAULT_LLM_MODEL.to_string());
        assert_eq!(DEFAULT_LLM_MODEL, "gemini-latest");
        assert_eq!(
            resolve_llm_model(
                Some("feature-model".to_string()),
                Some("shared-model".to_string()),
            ),
            "feature-model"
        );
        assert_eq!(
            resolve_llm_model(None, Some("shared-model".to_string())),
            "shared-model"
        );
    }

    #[test]
    fn llm_feature_validation_is_independent() {
        let mut config = super::AppConfig::load();
        config.tera_api_key = "test-key".to_string();
        config.tera_api_base = "https://example.test/v1".to_string();
        config.grammar_model = "grammar-model".to_string();
        config.question_model = String::new();

        assert!(config.validate_for_grammar().is_ok());
        assert_eq!(
            config.validate_for_question().unwrap_err().to_string(),
            "TERA_QUESTION_MODEL is empty"
        );
    }

    #[test]
    fn worker_endpoints_derive_from_one_https_origin() {
        assert_eq!(
            worker_endpoint_url("https://worker.example.test/", "/v1/pastes").unwrap(),
            "https://worker.example.test/v1/pastes"
        );
        assert!(worker_endpoint_url("http://worker.example.test", "/v1/pastes").is_err());
        assert!(worker_endpoint_url("https://worker.example.test/api", "/v1/pastes").is_err());
    }
}

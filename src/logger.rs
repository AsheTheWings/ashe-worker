use opentelemetry::logs::{AnyValue, LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use std::path::PathBuf;
use std::sync::OnceLock;

static LOGGER_PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();

pub fn install(provider: SdkLoggerProvider) {
    let _ = LOGGER_PROVIDER.set(provider);
}

pub fn info(message: impl AsRef<str>) {
    let Some(provider) = LOGGER_PROVIDER.get() else {
        return;
    };
    let (operation, outcome, severity) = classify(message.as_ref());
    let logger = provider.logger("ashe-worker");
    let mut record = logger.create_log_record();
    record.set_event_name("ashe.worker.operation");
    record.set_severity_number(severity);
    record.set_body(AnyValue::String("ashe.worker.operation".into()));
    record.add_attribute("ashe.worker.operation", operation);
    record.add_attribute("ashe.worker.outcome", outcome);
    if outcome == "error" {
        record.add_attribute("error.type", "operational");
    }
    logger.emit(record);
}

pub fn remove_legacy_file() {
    let _ = std::fs::remove_file(legacy_log_path());
}

fn legacy_log_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("ashe-worker.log")))
        .unwrap_or_else(|| PathBuf::from("ashe-worker.log"))
}

fn classify(message: &str) -> (&'static str, &'static str, Severity) {
    let lower = message.to_ascii_lowercase();
    let operation = if lower.contains("voice") {
        "voice"
    } else if lower.contains("dictation") || lower.contains("transcription") {
        "dictation"
    } else if lower.contains("audio") {
        "audio"
    } else if lower.contains("hotkey") || lower.contains("keyboard") {
        "input"
    } else if lower.contains("clipboard") || lower.contains("paste") {
        "clipboard"
    } else if lower.contains("archive") {
        "archive"
    } else if lower.contains("activity") || lower.contains("report") || lower.contains("journal") {
        "activity"
    } else if lower.contains("overlay") || lower.contains("window") {
        "interface"
    } else if lower.contains("config") {
        "configuration"
    } else if lower.contains("text action")
        || lower.contains("grammar")
        || lower.contains("question")
    {
        "text_action"
    } else {
        "runtime"
    };
    if lower.contains("failed") || lower.contains("error") || lower.contains("panic") {
        (operation, "error", Severity::Error)
    } else if lower.contains("ignored")
        || lower.contains("unavailable")
        || lower.contains("disabled")
        || lower.contains("already")
        || lower.contains("threshold")
    {
        (operation, "rejected", Severity::Warn)
    } else {
        (operation, "success", Severity::Info)
    }
}

#[cfg(test)]
mod tests {
    use super::classify;
    use opentelemetry::logs::Severity;

    #[test]
    fn classification_is_bounded_and_does_not_return_source_text() {
        assert_eq!(
            classify("Ashe voice failed: bearer secret and local path"),
            ("voice", "error", Severity::Error)
        );
        assert_eq!(
            classify("Clipboard image paste ignored while busy"),
            ("clipboard", "rejected", Severity::Warn)
        );
    }
}

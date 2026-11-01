use opentelemetry::logs::{AnyValue, LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use std::sync::OnceLock;

static LOGGER_PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();

pub fn install(provider: SdkLoggerProvider) {
    let _ = LOGGER_PROVIDER.set(provider);
}

/// Emit one operation event. The message only selects the bounded operation
/// and outcome attributes; its text is never exported.
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

fn classify(message: &str) -> (&'static str, &'static str, Severity) {
    let lower = message.to_ascii_lowercase();
    let operation = if lower.contains("hotkey") || lower.contains("keyboard") {
        "input"
    } else if lower.contains("clipboard") || lower.contains("injection") {
        "clipboard"
    } else if lower.contains("record upload") {
        "records"
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
mod unit {
    use super::classify;
    use opentelemetry::logs::Severity;

    #[test]
    fn classification_is_bounded_and_does_not_return_source_text() {
        assert_eq!(
            classify("Record upload scan failed: bearer secret and local path"),
            ("records", "error", Severity::Error)
        );
        assert_eq!(
            classify("Text action ignored; not idle"),
            ("text_action", "rejected", Severity::Warn)
        );
    }
}

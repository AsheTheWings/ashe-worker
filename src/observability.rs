//! OpenTelemetry composition root: providers, exporters, and resource identity.

use crate::config::{Exporter, TelemetryConfig};
use opentelemetry::logs::{AnyValue, LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry::metrics::MeterProvider;
use opentelemetry::trace::{Span, Tracer, TracerProvider};
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::{
    LogExporter, MetricExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::collections::HashMap;
use std::time::Duration;

const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Telemetry {
    traces: Option<SdkTracerProvider>,
    metrics: Option<SdkMeterProvider>,
    logs: Option<SdkLoggerProvider>,
}

impl Telemetry {
    /// Build the providers the exporter inputs select. An exporter that fails
    /// to build is a startup error, so a misconfigured process never runs blind.
    pub fn initialize(config: &TelemetryConfig, revision: &str) -> anyhow::Result<Self> {
        let resource = Resource::builder()
            .with_attributes([
                KeyValue::new("service.namespace", "ashe"),
                KeyValue::new("service.name", "ashe-worker"),
                KeyValue::new("service.version", revision.to_string()),
                KeyValue::new(
                    "service.instance.id",
                    format!("{:032x}", rand::random::<u128>()),
                ),
                KeyValue::new(
                    "deployment.environment.name",
                    config.deployment_environment.clone(),
                ),
            ])
            .build();
        let headers = parse_headers(&config.headers);
        let endpoint = |signal: &str| format!("{}/v1/{signal}", config.endpoint);

        let traces = match config.traces {
            Exporter::None => None,
            Exporter::Console => Some(
                SdkTracerProvider::builder()
                    .with_resource(resource.clone())
                    .with_simple_exporter(opentelemetry_stdout::SpanExporter::default())
                    .build(),
            ),
            Exporter::Otlp => Some(
                SdkTracerProvider::builder()
                    .with_resource(resource.clone())
                    .with_batch_exporter(
                        SpanExporter::builder()
                            .with_http()
                            .with_protocol(Protocol::HttpBinary)
                            .with_endpoint(endpoint("traces"))
                            .with_headers(headers.clone())
                            .with_timeout(EXPORT_TIMEOUT)
                            .build()?,
                    )
                    .build(),
            ),
        };
        let metrics = match config.metrics {
            Exporter::None => None,
            Exporter::Console => Some(
                SdkMeterProvider::builder()
                    .with_resource(resource.clone())
                    .with_periodic_exporter(opentelemetry_stdout::MetricExporter::default())
                    .build(),
            ),
            Exporter::Otlp => Some(
                SdkMeterProvider::builder()
                    .with_resource(resource.clone())
                    .with_periodic_exporter(
                        MetricExporter::builder()
                            .with_http()
                            .with_protocol(Protocol::HttpBinary)
                            .with_endpoint(endpoint("metrics"))
                            .with_headers(headers.clone())
                            .with_timeout(EXPORT_TIMEOUT)
                            .build()?,
                    )
                    .build(),
            ),
        };
        let logs = match config.logs {
            Exporter::None => None,
            Exporter::Console => Some(
                SdkLoggerProvider::builder()
                    .with_resource(resource)
                    .with_simple_exporter(opentelemetry_stdout::LogExporter::default())
                    .build(),
            ),
            Exporter::Otlp => Some(
                SdkLoggerProvider::builder()
                    .with_resource(resource)
                    .with_batch_exporter(
                        LogExporter::builder()
                            .with_http()
                            .with_protocol(Protocol::HttpBinary)
                            .with_endpoint(endpoint("logs"))
                            .with_headers(headers)
                            .with_timeout(EXPORT_TIMEOUT)
                            .build()?,
                    )
                    .build(),
            ),
        };

        global::set_text_map_propagator(TraceContextPropagator::new());
        if let Some(provider) = &traces {
            global::set_tracer_provider(provider.clone());
        }
        if let Some(provider) = &metrics {
            global::set_meter_provider(provider.clone());
        }
        if let Some(provider) = &logs {
            crate::logger::install(provider.clone());
        }
        let telemetry = Self {
            traces,
            metrics,
            logs,
        };
        telemetry.lifecycle("start");
        Ok(telemetry)
    }

    fn lifecycle(&self, phase: &'static str) {
        if let Some(provider) = &self.traces {
            let mut span = provider
                .tracer("ashe-worker")
                .start("ashe.worker.lifecycle");
            span.set_attribute(KeyValue::new("ashe.worker.lifecycle.phase", phase));
            span.end();
        }
        if let Some(provider) = &self.metrics {
            provider
                .meter("ashe-worker")
                .u64_counter("ashe.worker.lifecycle.count")
                .with_unit("1")
                .build()
                .add(1, &[KeyValue::new("ashe.worker.lifecycle.phase", phase)]);
        }
        if let Some(provider) = &self.logs {
            let logger = provider.logger("ashe-worker");
            let mut record = logger.create_log_record();
            record.set_event_name("ashe.worker.lifecycle");
            record.set_severity_number(Severity::Info);
            record.set_body(AnyValue::String("ashe.worker.lifecycle".into()));
            record.add_attribute("ashe.worker.lifecycle.phase", phase);
            logger.emit(record);
        }
    }

    /// Emit the qualification canary once: a span and a log record carrying
    /// one correlation identifier, and one counter increment.
    pub fn emit_qualification(&self) {
        let correlation_id = format!("{:032x}", rand::random::<u128>());
        if let Some(provider) = &self.traces {
            let mut span = provider
                .tracer("ashe-worker")
                .start("ashe.worker.qualification.canary");
            span.set_attribute(KeyValue::new(
                "ashe.worker.telemetry.correlation_id",
                correlation_id.clone(),
            ));
            span.end();
        }
        if let Some(provider) = &self.metrics {
            provider
                .meter("ashe-worker")
                .u64_counter("ashe.worker.qualification.canary.count")
                .with_unit("1")
                .build()
                .add(1, &[]);
        }
        if let Some(provider) = &self.logs {
            let logger = provider.logger("ashe-worker");
            let mut record = logger.create_log_record();
            record.set_event_name("ashe.worker.qualification.canary");
            record.set_severity_number(Severity::Info);
            record.set_body(AnyValue::String("ashe.worker.qualification.canary".into()));
            record.add_attribute(
                "ashe.worker.telemetry.correlation_id",
                correlation_id.clone(),
            );
            logger.emit(record);
        }
        println!("correlation id: {correlation_id}");
    }

    /// Flush and stop every provider.
    pub fn shutdown(self) {
        self.lifecycle("stop");
        if let Some(provider) = self.traces {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.metrics {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.logs {
            let _ = provider.shutdown();
        }
    }
}

/// Parse `OTEL_EXPORTER_OTLP_HEADERS`: comma-separated `key=value` pairs whose
/// values are percent-decoded, as the OpenTelemetry environment specification defines.
fn parse_headers(raw: &str) -> HashMap<String, String> {
    raw.split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = key.trim();
            (!key.is_empty()).then(|| (key.to_string(), percent_decode(value.trim())))
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|digits| u8::from_str_radix(digits, 16).ok())
        {
            output.push(byte);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(test)]
mod unit {
    use super::parse_headers;

    #[test]
    fn headers_are_split_and_percent_decoded() {
        let headers = parse_headers("Authorization=Basic%20c3ludGhldGlj, x-extra = 1");
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Basic c3ludGhldGlj")
        );
        assert_eq!(headers.get("x-extra").map(String::as_str), Some("1"));
        assert!(parse_headers("").is_empty());
    }
}

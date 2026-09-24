use opentelemetry::logs::{AnyValue, LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry::metrics::MeterProvider;
use opentelemetry::trace::{Span, Tracer, TracerProvider};
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::{LogExporter, MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;

pub struct Telemetry {
    traces: Option<SdkTracerProvider>,
    metrics: Option<SdkMeterProvider>,
    logs: Option<SdkLoggerProvider>,
}

impl Telemetry {
    pub fn initialize(version: &str, build_id: &str) -> Self {
        if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .ok()
            .filter(|v| !v.is_empty())
            .is_none()
            || std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
                .ok()
                .filter(|v| !v.is_empty())
                .is_none()
        {
            return Self {
                traces: None,
                metrics: None,
                logs: None,
            };
        }
        let environment = std::env::var("ASHE_WORKER_ENV").unwrap_or_else(|_| "development".into());
        let resource = Resource::builder()
            .with_attributes([
                KeyValue::new("service.namespace", "ashe"),
                KeyValue::new("service.name", "ashe-worker"),
                KeyValue::new("service.version", build_id.to_string()),
                KeyValue::new("service.instance.id", process_instance_id()),
                KeyValue::new("deployment.environment.name", environment),
                KeyValue::new("ashe.worker.app_version", version.to_string()),
            ])
            .build();

        let traces = SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
            .ok()
            .map(|exporter| {
                SdkTracerProvider::builder()
                    .with_resource(resource.clone())
                    .with_batch_exporter(exporter)
                    .build()
            });
        let metrics = MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
            .ok()
            .map(|exporter| {
                SdkMeterProvider::builder()
                    .with_resource(resource.clone())
                    .with_periodic_exporter(exporter)
                    .build()
            });
        let logs = LogExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
            .ok()
            .map(|exporter| {
                SdkLoggerProvider::builder()
                    .with_resource(resource)
                    .with_batch_exporter(exporter)
                    .build()
            });

        if let Some(provider) = &traces {
            global::set_tracer_provider(provider.clone());
            let tracer = provider.tracer("ashe-worker");
            let mut span = tracer.start("ashe.worker.lifecycle.start");
            span.set_attribute(KeyValue::new("ashe.worker.lifecycle.phase", "start"));
            span.end();
        }
        if let Some(provider) = &metrics {
            global::set_meter_provider(provider.clone());
            provider
                .meter("ashe-worker")
                .u64_counter("ashe.worker.lifecycle.count")
                .build()
                .add(1, &[KeyValue::new("ashe.worker.lifecycle.phase", "start")]);
        }
        if let Some(provider) = &logs {
            let logger = provider.logger("ashe-worker");
            let mut record = logger.create_log_record();
            record.set_event_name("ashe.worker.service.started");
            record.set_severity_number(Severity::Info);
            record.set_body(AnyValue::String("ashe.worker.service.started".into()));
            logger.emit(record);
        }
        Self {
            traces,
            metrics,
            logs,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.traces.is_some() && self.metrics.is_some() && self.logs.is_some()
    }

    pub fn emit_qualification(&self) {
        let correlation_id = process_instance_id();
        if let Some(provider) = &self.traces {
            let tracer = provider.tracer("ashe-worker");
            let mut span = tracer.start("ashe.worker.qualification.canary");
            span.set_attribute(KeyValue::new(
                "ashe.worker.telemetry.correlation_id",
                correlation_id.clone(),
            ));
            span.set_attribute(KeyValue::new(
                "ashe.worker.lifecycle.phase",
                "qualification",
            ));
            span.end();
        }
        if let Some(provider) = &self.metrics {
            provider
                .meter("ashe-worker")
                .u64_counter("ashe.worker.qualification.canary.count")
                .build()
                .add(1, &[]);
        }
        if let Some(provider) = &self.logs {
            let logger = provider.logger("ashe-worker");
            let mut record = logger.create_log_record();
            record.set_event_name("ashe.worker.qualification.canary");
            record.set_severity_number(Severity::Info);
            record.set_body(AnyValue::String("ashe.worker.qualification.canary".into()));
            record.add_attribute("ashe.worker.telemetry.correlation_id", correlation_id);
            record.add_attribute("ashe.worker.lifecycle.phase", "qualification");
            logger.emit(record);
        }
    }

    pub fn shutdown(self) {
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

fn process_instance_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

use opentelemetry::logs::{AnyValue, LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry::metrics::MeterProvider;
use opentelemetry::propagation::Injector;
use opentelemetry::trace::{Span, SpanKind, Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Context, KeyValue, global};
use opentelemetry_otlp::{LogExporter, MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
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
            global::set_text_map_propagator(TraceContextPropagator::new());
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
            crate::logger::install(provider.clone());
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

struct HeaderInjector<'a>(&'a mut reqwest::header::HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        let Ok(name) = reqwest::header::HeaderName::from_bytes(key.as_bytes()) else {
            return;
        };
        let Ok(value) = reqwest::header::HeaderValue::from_str(&value) else {
            return;
        };
        self.0.insert(name, value);
    }
}

pub struct VoiceSessionTelemetry {
    context: Context,
    ended: bool,
}

impl VoiceSessionTelemetry {
    pub fn start() -> Self {
        let tracer = global::tracer("ashe-worker");
        let mut span = tracer
            .span_builder("ashe.worker.voice.session")
            .with_kind(SpanKind::Internal)
            .start(&tracer);
        span.set_attribute(KeyValue::new("ashe.worker.operation", "voice.session"));
        Self {
            context: Context::current_with_span(span),
            ended: false,
        }
    }

    pub fn phase(&self, phase: &'static str) {
        self.context.span().add_event(
            "ashe.worker.voice.phase",
            vec![KeyValue::new("ashe.worker.voice.phase", phase)],
        );
    }

    pub fn dependency(
        &self,
        method: &'static str,
        route: &'static str,
    ) -> VoiceDependencyTelemetry {
        let tracer = global::tracer("ashe-worker");
        let mut span = tracer
            .span_builder("ashe.worker.assistant.request")
            .with_kind(SpanKind::Client)
            .start_with_context(&tracer, &self.context);
        span.set_attribute(KeyValue::new("ashe.worker.operation", "assistant.request"));
        span.set_attribute(KeyValue::new("http.request.method", method));
        span.set_attribute(KeyValue::new("http.route", route));
        VoiceDependencyTelemetry {
            context: Context::current_with_span(span),
            ended: false,
        }
    }

    pub fn finish(mut self, success: bool, reason: &'static str) {
        self.end(success, reason);
    }

    fn end(&mut self, success: bool, reason: &'static str) {
        if self.ended {
            return;
        }
        self.ended = true;
        let span = self.context.span();
        span.set_attribute(KeyValue::new(
            "ashe.worker.outcome",
            if success { "success" } else { "error" },
        ));
        span.set_attribute(KeyValue::new("ashe.worker.voice.close_reason", reason));
        if success {
            span.set_status(Status::Ok);
        } else {
            span.set_attribute(KeyValue::new("error.type", "operational"));
            span.set_status(Status::error("operational"));
        }
        span.end();
    }
}

pub struct VoiceDependencyTelemetry {
    context: Context,
    ended: bool,
}

impl VoiceDependencyTelemetry {
    pub fn inject(&self, headers: &mut reqwest::header::HeaderMap) {
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&self.context, &mut HeaderInjector(headers));
        });
    }

    pub fn finish(
        mut self,
        status_code: Option<u16>,
        outcome: &'static str,
        error_type: Option<&'static str>,
    ) {
        self.end(status_code, outcome, error_type);
    }

    fn end(
        &mut self,
        status_code: Option<u16>,
        outcome: &'static str,
        error_type: Option<&'static str>,
    ) {
        if self.ended {
            return;
        }
        self.ended = true;
        let span = self.context.span();
        span.set_attribute(KeyValue::new("ashe.worker.outcome", outcome));
        if let Some(status_code) = status_code {
            span.set_attribute(KeyValue::new(
                "http.response.status_code",
                i64::from(status_code),
            ));
        }
        if let Some(error_type) = error_type {
            span.set_attribute(KeyValue::new("error.type", error_type));
            span.set_status(Status::error(error_type));
        } else {
            span.set_status(Status::Ok);
        }
        span.end();
    }
}

impl Drop for VoiceDependencyTelemetry {
    fn drop(&mut self) {
        self.end(None, "error", Some("dependency"));
    }
}

impl Drop for VoiceSessionTelemetry {
    fn drop(&mut self) {
        self.end(false, "operational_error");
    }
}

fn process_instance_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

#[cfg(test)]
mod tests {
    use super::HeaderInjector;
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use opentelemetry::{Context, global};
    use opentelemetry_sdk::propagation::TraceContextPropagator;

    #[test]
    fn injects_w3c_trace_context_into_http_headers() {
        global::set_text_map_propagator(TraceContextPropagator::new());
        let span_context = SpanContext::new(
            TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap(),
            SpanId::from_hex("b7ad6b7169203331").unwrap(),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        let context = Context::new().with_remote_span_context(span_context);
        let mut headers = reqwest::header::HeaderMap::new();
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&context, &mut HeaderInjector(&mut headers));
        });
        assert_eq!(
            headers.get("traceparent").unwrap().to_str().unwrap(),
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
        );
    }
}

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
use opentelemetry_sdk::logs::{self, BatchLogProcessor, SdkLoggerProvider};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{self, BatchSpanProcessor, SdkTracerProvider};
use std::collections::HashMap;
use std::time::Duration;

const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// Spans or log records per OTLP request, which keeps each request body to
/// tens of kilobytes.
const EXPORT_BATCH_SIZE: usize = 64;

fn span_batch(exporter: impl trace::SpanExporter + 'static) -> BatchSpanProcessor {
    BatchSpanProcessor::builder(exporter)
        .with_batch_config(
            trace::BatchConfigBuilder::default()
                .with_max_export_batch_size(EXPORT_BATCH_SIZE)
                .build(),
        )
        .build()
}

fn log_batch(exporter: impl logs::LogExporter + 'static) -> BatchLogProcessor {
    BatchLogProcessor::builder(exporter)
        .with_batch_config(
            logs::BatchConfigBuilder::default()
                .with_max_export_batch_size(EXPORT_BATCH_SIZE)
                .build(),
        )
        .build()
}

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
                    .with_span_processor(span_batch(
                        SpanExporter::builder()
                            .with_http()
                            .with_protocol(Protocol::HttpBinary)
                            .with_endpoint(endpoint("traces"))
                            .with_headers(headers.clone())
                            .with_timeout(EXPORT_TIMEOUT)
                            .build()?,
                    ))
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
                    .with_log_processor(log_batch(
                        LogExporter::builder()
                            .with_http()
                            .with_protocol(Protocol::HttpBinary)
                            .with_endpoint(endpoint("logs"))
                            .with_headers(headers)
                            .with_timeout(EXPORT_TIMEOUT)
                            .build()?,
                    ))
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

#[cfg(test)]
mod component {
    use super::{EXPORT_BATCH_SIZE, log_batch, span_batch};
    use opentelemetry::logs::{LogRecord, Logger, LoggerProvider};
    use opentelemetry::trace::{Span, Tracer, TracerProvider};
    use opentelemetry_otlp::{LogExporter, Protocol, SpanExporter, WithExportConfig};
    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use prost::Message;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    type Received = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// The SDK reads `OTEL_` names from the environment itself, so a test run
    /// with any of them set would neither be hermetic nor keep its headers.
    fn refuse_otel_environment() {
        let names: Vec<String> = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| name.to_ascii_uppercase().starts_with("OTEL_"))
            .collect();
        assert!(
            names.is_empty(),
            "unset {names:?} to run the telemetry component tests"
        );
    }

    /// A loopback OTLP/HTTP receiver that records each request's path and
    /// body and answers every one with an empty success. Dropping it stops
    /// accepting connections.
    struct Receiver {
        origin: String,
        received: Received,
        stopped: Arc<AtomicBool>,
        accepting: Option<JoinHandle<()>>,
    }

    impl Receiver {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let received = Received::default();
            let stopped = Arc::new(AtomicBool::new(false));
            let (sink, stop) = (received.clone(), stopped.clone());
            let accepting = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(stream) = stream else { return };
                    let sink = sink.clone();
                    std::thread::spawn(move || serve(stream, &sink));
                }
            });
            Self {
                origin,
                received,
                stopped,
                accepting: Some(accepting),
            }
        }
    }

    impl Drop for Receiver {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            // Wake the accept loop so it sees the stop flag.
            let _ = TcpStream::connect(self.origin.trim_start_matches("http://"));
            if let Some(accepting) = self.accepting.take() {
                let _ = accepting.join();
            }
        }
    }

    fn serve(stream: TcpStream, sink: &Received) {
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        loop {
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                return;
            }
            let path = request_line.split(' ').nth(1).unwrap_or("").to_string();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            sink.lock().unwrap().push((path, body));
            writer
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\n\r\n")
                .unwrap();
        }
    }

    fn bodies(receiver: &Receiver, path: &str) -> Vec<Vec<u8>> {
        receiver
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|(candidate, _)| candidate == path)
            .map(|(_, body)| body.clone())
            .collect()
    }

    const EMITTED: usize = EXPORT_BATCH_SIZE + 1;

    #[test]
    fn each_span_request_holds_at_most_one_batch() {
        refuse_otel_environment();
        let receiver = Receiver::start();
        let exporter = SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(format!("{}/v1/traces", receiver.origin))
            .build()
            .unwrap();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(span_batch(exporter))
            .build();
        let tracer = provider.tracer("component");
        for _ in 0..EMITTED {
            tracer.start("component.span").end();
        }
        provider.shutdown().unwrap();

        let counts: Vec<usize> = bodies(&receiver, "/v1/traces")
            .iter()
            .map(|body| {
                ExportTraceServiceRequest::decode(body.as_slice())
                    .unwrap()
                    .resource_spans
                    .iter()
                    .flat_map(|resource| &resource.scope_spans)
                    .map(|scope| scope.spans.len())
                    .sum()
            })
            .collect();
        assert_eq!(counts.iter().sum::<usize>(), EMITTED);
        assert!(
            counts.iter().all(|count| *count <= EXPORT_BATCH_SIZE),
            "{counts:?}"
        );
    }

    #[test]
    fn each_log_request_holds_at_most_one_batch() {
        refuse_otel_environment();
        let receiver = Receiver::start();
        let exporter = LogExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(format!("{}/v1/logs", receiver.origin))
            .build()
            .unwrap();
        let provider = SdkLoggerProvider::builder()
            .with_log_processor(log_batch(exporter))
            .build();
        let logger = provider.logger("component");
        for _ in 0..EMITTED {
            let mut record = logger.create_log_record();
            record.set_event_name("component.record");
            logger.emit(record);
        }
        provider.shutdown().unwrap();

        let counts: Vec<usize> = bodies(&receiver, "/v1/logs")
            .iter()
            .map(|body| {
                ExportLogsServiceRequest::decode(body.as_slice())
                    .unwrap()
                    .resource_logs
                    .iter()
                    .flat_map(|resource| &resource.scope_logs)
                    .map(|scope| scope.log_records.len())
                    .sum()
            })
            .collect();
        assert_eq!(counts.iter().sum::<usize>(), EMITTED);
        assert!(
            counts.iter().all(|count| *count <= EXPORT_BATCH_SIZE),
            "{counts:?}"
        );
    }
}

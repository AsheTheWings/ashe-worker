# Observability

The worker exports diagnostics only as OpenTelemetry traces, metrics, and log
records. It writes no log file. Message boxes and command-line output are
user-facing results, not telemetry.

## Export

`OTEL_TRACES_EXPORTER`, `OTEL_METRICS_EXPORTER`, and `OTEL_LOGS_EXPORTER`
select `otlp`, `console`, or `none` per signal. OTLP uses HTTP with protobuf;
each signal goes to `<OTEL_EXPORTER_OTLP_ENDPOINT>/v1/<signal>` with the
protected `OTEL_EXPORTER_OTLP_HEADERS`. Exports time out after 10 seconds and
flush when the worker exits. A failed export is dropped, with no fallback.

Every signal carries `service.namespace` `ashe`, `service.name`
`ashe-worker`, `service.version` set to the build revision, a random
`service.instance.id` per process, and `deployment.environment.name` from
`ASHE_WORKER_DEPLOYMENT_ENVIRONMENT`.

## Signals

[`observability/signal-catalog.json`](../observability/signal-catalog.json)
lists every span, metric, log record, attribute, attribute value, and error
class the worker emits; a conformance test keeps it equal to the code.

| Signal | Name | Meaning |
| --- | --- | --- |
| span, log, counter | `ashe.worker.lifecycle`, `ashe.worker.lifecycle.count` | start and stop, by `ashe.worker.lifecycle.phase` |
| span | `ashe.worker.records.scan` | one upload scan, with its outcome, counts, and `error.type` |
| log | `ashe.worker.operation` | one event by bounded operation and outcome; message text is never exported |
| span, log, counter | `ashe.worker.qualification.canary`, `ashe.worker.qualification.canary.count` | the qualification canary |

Upload failures map to the `error.type` classes `dependency`,
`unauthorized`, `conflict`, `rejected`, and `operational`.

## Qualification

`ashe-worker.exe --qualify-observability` loads the inputs, emits the canary
once, flushes, and prints the canary's
`ashe.worker.telemetry.correlation_id` so its span and log record can be
found at the destination.

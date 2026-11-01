# Testing

The fast gate runs every test that needs no Windows desktop session:

```sh
cargo test --locked
```

Tests read no environment variable or user file; they build their inputs and
configuration explicitly with synthetic credentials. The OpenTelemetry SDK
reads `OTEL_` names itself, so the telemetry component tests refuse to run
while any is set.

| Class | Boundary | Command |
| --- | --- | --- |
| unit | pure functions, in `mod unit` | `cargo test --locked unit::` |
| component | the filesystem and loopback HTTP, in `mod component` | `cargo test --locked component::` |
| conformance | `.env` and the signal catalog against the code, in `tests/conformance.rs` | `cargo test --locked --test conformance` |
| qualification | the built executable | `./scripts/qualify.ps1 -Executable <exe> -Revision <revision>` |

Library tests also run on Linux. Win32 code compiles and its tests run on
Windows only.

Qualification runs the executable with synthetic inputs only. It checks the
stamped revision, accepts valid inputs, refuses a missing protected file, and
exports the telemetry canary to the console.

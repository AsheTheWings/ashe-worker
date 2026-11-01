# Configuration

Every input is an environment-style `NAME=value` setting. The input schema in
`src/inputs.rs` declares each one; the generated [`.env`](../.env) lists them
all with their shapes and development defaults and is never loaded.
Regenerate it after a schema change:

```sh
cargo run --bin ashe-worker-env > .env
```

## Layers

The worker merges four layers, lowest first, then validates the result before
it starts any work:

1. The schema defaults of the selected profile.
2. The overlay file, `%APPDATA%\ashe-worker\config.env` by default.
3. The protected file, `%APPDATA%\ashe-worker\protected.env` by default.
4. The process environment.

`ASHE_WORKER_PROFILE` selects `development` or `production` defaults and has
no default of its own. An input the selected profile gives no default is
required. `ASHE_WORKER_OVERLAY_FILE` and `ASHE_WORKER_PROTECTED_FILE` move
the two files and are read from the process environment only.

An absent file contributes nothing. A file that is present but unreadable or
malformed stops the worker, and so does an undeclared `ASHE_WORKER_` name, a
name set twice in one file, or a value of the wrong shape. Errors name the
inputs, never their values. At startup the worker shows them in a message
box; `ashe-worker.exe --check-inputs` prints them and exits with status 2.

## Files

The overlay file holds configuration inputs, one `NAME=value` per line.
Blank lines and lines starting with `#` are ignored, and a value may be
wrapped in single quotes.

The protected file holds the protected inputs: the Responses API key, the
records server token, and the OTLP headers. They are accepted from no other
layer. Write each as `NAME='value'`. The worker refuses the file unless its
access list grants access to the current user, `SYSTEM`, and
`Administrators` alone. To restrict a new file:

```powershell
icacls "$env:APPDATA\ashe-worker\protected.env" /inheritance:r /grant:r "${env:USERNAME}:F"
```

A minimal production overlay names the profile, the Responses API, the
models, the records server, and the telemetry endpoint:

```ini
ASHE_WORKER_PROFILE=production
ASHE_WORKER_LLM_BASE_URL=https://responses.example/v1
ASHE_WORKER_GRAMMAR_MODEL=<model>
ASHE_WORKER_QUESTION_MODEL=<model>
ASHE_WORKER_JOURNAL_MODEL=<model that accepts images>
ASHE_WORKER_SERVER_URL=http://records.example:8080
OTEL_EXPORTER_OTLP_ENDPOINT=http://collector.example:4318
```

Path defaults use `%APPDATA%` and `%LOCALAPPDATA%`, which resolve to the
user's roaming and local application data folders.

## Reloading

The tray menu's "Reload config" reads every layer again. Invalid inputs keep
the running configuration and show the errors. Telemetry inputs apply at the
next start.

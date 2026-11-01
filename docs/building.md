# Building

CI builds with Rust 1.98.0 on Windows. To build locally on Windows:

```powershell
cargo build --release --locked --bin ashe-worker
```

The build stamps `ASHE_WORKER_REVISION` into the executable, or the Git
revision with a `-dirty` suffix for a modified tree, or `unknown` outside
Git. `ashe-worker.exe --version` prints it.

## CI artifacts

Each CI run on a pull request, on a push to `dev`, or by manual dispatch tests
the source, builds the release executable with the commit as its revision,
qualifies that executable with `scripts/qualify.ps1`, and keeps it for 30
days as the run artifact `ashe-worker-<commit>`. The run summary records the
executable's SHA-256.

## Updating an installed copy

Take the executable from a successful run on `dev`:

```powershell
gh run download <run-id> --repo <owner>/ashe-worker --name ashe-worker-<commit>
(Get-FileHash -Algorithm SHA256 .\ashe-worker.exe).Hash.ToLowerInvariant()
```

Compare the digest with the run summary, quit the running worker from its
tray menu, replace `%LOCALAPPDATA%\Programs\ashe-worker\ashe-worker.exe`, and
start it again. Check `--version` against the commit.

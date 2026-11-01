# Ashe Worker

Ashe Worker is a Windows tray application for quick text assistance and a
personal journal of computer activity. It corrects or answers questions about
selected text through any OpenAI Responses API, describes what happens on
screen in ten-minute blocks, and sends each finished day to a records server.

The repository publishes one artifact: `ashe-worker.exe`, built and qualified
by CI on every pull request and on every push to `dev`.

## Features

- Correct the selected text in place (`Win+Shift+G`).
- Ask a question about the selected text and append the answer after it
  (`Win+Shift+Q`).
- Describe activity and learning per block, keep a rolling summary and a
  chronological journal, and write a report for each finished day.
- Skip configured applications and pause while Windows is locked.
- Upload each finished day to a records server, then remove it locally after
  a retention window.

While a text action runs, a small `processing` pill follows the cursor;
`Esc` cancels it. Only `Esc` is captured, so typing keeps reaching the
foreground application.

## Setup

1. Get `ashe-worker.exe` as [building](docs/building.md) describes and place
   it in `%LOCALAPPDATA%\Programs\ashe-worker\`.
2. Write the overlay file `%APPDATA%\ashe-worker\config.env` and the
   protected file `%APPDATA%\ashe-worker\protected.env` as
   [configuration](docs/configuration.md) describes.
3. Check the inputs from a terminal:

   ```powershell
   & "$env:LOCALAPPDATA\Programs\ashe-worker\ashe-worker.exe" --check-inputs
   ```

4. Launch `ashe-worker.exe`. To start it at sign-in, place a shortcut to it
   in `shell:startup`.

The tray icon's left click opens the records folder; its menu pauses or
resumes activity tracking, opens today's journal, reloads the configuration,
and quits.

## Documentation

- [Configuration](docs/configuration.md): inputs, their layers, and file locations.
- [Records](docs/records.md): the data directory and the upload contract.
- [Observability](docs/observability.md): telemetry export and the signal catalog.
- [Building](docs/building.md): toolchain, CI artifacts, and updating an installed copy.
- [Testing](docs/testing.md): test classes and their commands.

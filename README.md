# Ashe Worker

Ashe Worker is a tray-based Windows companion for hands-free writing, quick text assistance, and
personal activity reflection. It combines speech transcription, contextual text actions, and an
automatic journal of computer activity and learning in one lightweight desktop application.

The project is intended for people who want useful context about how they spend time at their
computer without manually maintaining a work log. Reports remain available as local artifacts,
and older records can be placed in encrypted archives for retention or backup.

## Features

- Dictate into the active application.
- Correct selected text or ask a question about it.
- Create activity, learning, and daily reports automatically.
- Keep a rolling summary and chronological journal.
- Exclude configured applications from activity capture.
- Encrypt older records and optionally upload the encrypted archives.
- Upload a clipboard image and paste its remote path into the active application.

## Getting started

1. Build or obtain `ashe-worker.exe` and place it in a directory where it can keep its local
   configuration and artifacts.
2. Copy [.env.example](.env.example) to `.env` beside the executable or at the project root.
3. Add the credentials for the features you want to use and review the optional paths, privacy
   exclusions, per-feature LLM models, reporting, and archive settings.
4. Launch `ashe-worker.exe`.

The app starts in the Windows system tray. Left-click the tray icon to open the artifact
collection. Right-click it to control features, reload configuration, open logs, or quit.

## Hotkeys

| Hotkey | Action |
| --- | --- |
| `Win+Shift+H` | Start or stop dictation |
| `Win+Shift+G` | Correct selected text |
| `Win+Shift+Q` | Ask a question using selected text |
| `Ctrl+Alt+V` | Upload a clipboard image and paste its remote path |

Pressing `Esc` cancels an in-flight grammar or question request; only the
cancel key is captured, so typing keeps reaching the foreground app.

During dictation, microphone audio is buffered locally while the pill overlay
shows a live voice spectrum. Printable input and `Ctrl+V` switch to a private
typing buffer without writing into the foreground application. A status bar is
always rendered above the pill as one connected body, with no border across
the contact surface. Its left side always shows the insertion count. While
typing with content it shows a trailing preview, representing clipboard
spans as `[pasted]`; otherwise it shows a centered recording timer.
`Shift+Enter` inserts a line break without ending the session. While
listening, the bar shows `↵` at its right edge for 1.5 seconds; while typing,
`↵` stays inline in the editable preview and one `Backspace` removes it. The
marker renders larger in cyan, with horizontal padding in the typing preview.
Press `Enter` with content to commit it and resume listening. Press `Enter`
with an empty buffer to finish, or `Esc` to cancel the whole session.
Navigation and unrelated system shortcuts continue to work normally. Direct
IME and emoji-panel composition is not captured; use `Ctrl+V` for that
content.

Natural pauses remain intact. Once silence reaches five seconds, dead air is
compacted locally and the bar shows `silence skipped` instead of the timer;
compacted audio is never submitted as a separate request. When the session
finishes, all retained speech is sent once through the fal.ai queue API
(`FAL_STT_MODEL`, default scribe-v2), then interleaved with exact typed and
pasted content using word timestamps. The result is inserted once into the
application where dictation started, with no LLM polishing. While fal.ai is
working, the pill shows `processing...`. Dictation requires `FAL_KEY`.

Spoken punctuation is converted automatically: say `comma`, `period`,
`question mark`, `exclamation mark`, `colon`, `semicolon`, `double quote`,
`single quote`, `new line`, or `new paragraph` and the symbol is inserted
while the command words are dropped. Say `literal` before a command to keep
the words instead. Set `ASHE_SPOKEN_PUNCTUATION=0` to disable.

## Activity records

When enabled, activity tracking periodically observes the configured display and produces local
reports describing meaningful activity and learning. The artifact collection includes recent
reports, a chronological journal, a rolling summary, and daily overviews.

Review [.env.example](.env.example) to choose where records are stored, disable tracking, exclude
sensitive applications, or configure retention and encrypted backup. Activity capture pauses while
Windows is locked.

## Encrypted archives

The companion CLI can manage encrypted activity archives:

```powershell
.\ashe-worker-cli.exe archive seal <YYYY-MM-DD>
.\ashe-worker-cli.exe archive upload <archive.ashe>
.\ashe-worker-cli.exe archive decrypt <archive.ashe> <output-directory>
```

Archive recovery requires the passphrase created when the archive is sealed. Store that passphrase
safely; it cannot be recovered by the application.

## Building

Build the Windows application and CLI with Cargo:

```powershell
cargo build --release
```

Linux and WSL users can use the included cross-build helper after configuring the release settings
described in [.env.example](.env.example):

```bash
./scripts/ship-windows-release.sh
```

The helper validates the runtime `.env.local` already present in `ASHE_RELEASE_DIR`; it does not
copy the project `.env.local` or secrets into the release directory.

## Troubleshooting

Open the local log from the tray menu when a hotkey, microphone, network request, activity report,
or archive operation fails.

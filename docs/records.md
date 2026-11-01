# Records

## Data directory

`ASHE_WORKER_DATA_DIR`, `%LOCALAPPDATA%\ashe-worker` by default, holds:

| Path | Content |
| --- | --- |
| `<YYYY-MM-DD>/blocks/` | one JSON description per activity block |
| `<YYYY-MM-DD>/journal.md` | the day's chronological journal |
| `<YYYY-MM-DD>/daily.md` | the day's report, written after the day ends |
| `<YYYY-MM-DD>/frames/`, `<YYYY-MM-DD>/keyframes/` | screen captures, which never leave the machine |
| `pending/` | blocks waiting for their description |
| `summary.md`, `context-state.json` | the rolling summary and its state |
| `upload-ledger.json` | the acknowledged digest of each uploaded file |

## Upload

Every `ASHE_WORKER_UPLOAD_SCAN_MINUTES`, the worker looks for finished days.
A day is finished when it is before today, the daily report grace period has
passed, none of its blocks is pending, and its `daily.md` is current when
daily reports are enabled.

Each file of a finished day, except captures, dotfiles, and `.tmp` files, is
sent once:

```http
PUT <ASHE_WORKER_SERVER_URL>/v1/records/<YYYY-MM-DD>/<path>
Authorization: Bearer <ASHE_WORKER_SERVER_TOKEN>
Content-Type: application/octet-stream
X-Ashe-Record-Format: 1
X-Ashe-Record-Sha256: <lowercase hex SHA-256 of the body>
```

`<path>` is the file's path inside the day: 1 to 8 `/`-separated segments of
ASCII letters, digits, `.`, `_`, and `-`, each at most 128 bytes, none of them
`.` or `..`. A day holding any other path, or a link, is not uploaded.

The server stores each path once. It answers `201` for a new record, `200`
for an identical repeat, and `409` when it holds different content at the
path. The worker records each `200` or `201` in the ledger.

| Response | Effect |
| --- | --- |
| `200`, `201` | the file is acknowledged |
| `401`, `403` | the scan stops |
| `409` | the day stops; later days continue |
| other `4xx` | the day stops; later days continue |
| `5xx`, no response | the scan stops |

## Retention

A day is removed from the data directory once every file in it was
acknowledged with its current content and the day is more than
`ASHE_WORKER_LOCAL_RETENTION_DAYS` days old. A day with an unacknowledged or
changed file stays.

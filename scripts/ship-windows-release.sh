#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="x86_64-pc-windows-gnu"
BINARY="ashe-worker.exe"
CLI_BINARY="ashe-worker-cli.exe"
ENV_FILE="$PROJECT_ROOT/.env.local"

read_env() {
  local source_file="$1"
  local wanted="$2"
  local destination="$3"
  local line name parsed_value
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" == *"="* ]] || continue
    name="${line%%=*}"
    name="${name#"${name%%[![:space:]]*}"}"
    name="${name%"${name##*[![:space:]]}"}"
    [[ "$name" == "$wanted" ]] || continue
    parsed_value="${line#*=}"
    parsed_value="${parsed_value#"${parsed_value%%[![:space:]]*}"}"
    parsed_value="${parsed_value%"${parsed_value##*[![:space:]]}"}"
    if [[ "$parsed_value" == \"*\" && "$parsed_value" == *\" ]]; then
      parsed_value="${parsed_value:1:${#parsed_value}-2}"
    elif [[ "$parsed_value" == \'*\' && "$parsed_value" == *\' ]]; then
      parsed_value="${parsed_value:1:${#parsed_value}-2}"
    fi
    printf -v "$destination" '%s' "$parsed_value"
    return 0
  done < "$source_file"
  return 1
}

if [[ ! -f "$ENV_FILE" ]]; then
  echo ".env.local is required for release configuration" >&2
  exit 1
fi
RELEASE_DIR=""
RECIPIENT_FILE=""
read_env "$ENV_FILE" ASHE_RELEASE_DIR RELEASE_DIR || true
read_env "$ENV_FILE" ASHE_ARCHIVE_RECIPIENT_FILE RECIPIENT_FILE || true
if [[ -z "$RELEASE_DIR" ]]; then
  echo "ASHE_RELEASE_DIR is required in .env.local; release aborted" >&2
  exit 1
fi
if [[ -z "$RECIPIENT_FILE" ]]; then
  echo "ASHE_ARCHIVE_RECIPIENT_FILE is required in .env.local; release aborted" >&2
  exit 1
fi

RUNTIME_ENV_FILE="$RELEASE_DIR/.env.local"
if [[ ! -f "$RUNTIME_ENV_FILE" ]]; then
  echo "$RUNTIME_ENV_FILE is required for Windows runtime configuration; release aborted" >&2
  exit 1
fi
for name in ASHE_WORKER_BASE_URL ASHE_PASTE_UPLOAD_TOKEN \
  OTEL_EXPORTER_OTLP_ENDPOINT OTEL_EXPORTER_OTLP_HEADERS; do
  value=""
  read_env "$RUNTIME_ENV_FILE" "$name" value || true
  if [[ -z "$value" ]]; then
    echo "$name is required in $RUNTIME_ENV_FILE; release aborted" >&2
    exit 1
  fi
done

if [[ -n "${CARGO:-}" ]]; then
  CARGO_BIN="$CARGO"
elif command -v cargo >/dev/null 2>&1; then
  CARGO_BIN="$(command -v cargo)"
else
  echo "cargo was not found" >&2
  exit 1
fi

if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
  echo "x86_64-w64-mingw32-gcc was not found" >&2
  exit 1
fi

cd "$PROJECT_ROOT"
"$PROJECT_ROOT/scripts/check-observability.sh"
PACKAGE_VERSION="$(awk -F '"' '/^version = / { print $2; exit }' "$PROJECT_ROOT/Cargo.toml")"
GIT_SHA="$(git -C "$PROJECT_ROOT" rev-parse --short HEAD 2>/dev/null || echo nogit)"
if git -C "$PROJECT_ROOT" diff --quiet --ignore-submodules HEAD -- 2>/dev/null; then
  DIRTY=""
else
  DIRTY="-dirty"
fi
BUILD_ID="${ASHE_BUILD_ID:-v${PACKAGE_VERSION}+$(date -u +%Y%m%d%H%M%S)-${GIT_SHA}${DIRTY}}"
if [[ ! -f "$RECIPIENT_FILE" ]]; then
  echo "archive recipient was not found at $RECIPIENT_FILE" >&2
  exit 1
fi
ASHE_BUILD_ID="$BUILD_ID" ASHE_ARCHIVE_RECIPIENT_FILE="$RECIPIENT_FILE" \
  "$CARGO_BIN" build --release --target "$TARGET"
mkdir -p "$RELEASE_DIR"
rm -f "$RELEASE_DIR/ashe-worker.log"
cp "$PROJECT_ROOT/target/$TARGET/release/$BINARY" "$RELEASE_DIR/$BINARY"
cp "$PROJECT_ROOT/target/$TARGET/release/$CLI_BINARY" "$RELEASE_DIR/$CLI_BINARY"
cp "$PROJECT_ROOT/.env.example" "$RELEASE_DIR/.env.example"
printf '%s\n' "$BUILD_ID" > "$RELEASE_DIR/ashe-worker.build.txt"
printf 'Shipped %s and %s build_id=%s\n' \
  "$RELEASE_DIR/$BINARY" "$RELEASE_DIR/$CLI_BINARY" "$BUILD_ID"

#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if grep -RInE 'eprintln!|println!|tracing_subscriber|std::io::(stderr|stdout)' \
  "$root/src" --include='*.rs' --exclude='cli_main.rs'; then
  printf '%s\n' 'alternate runtime diagnostic sink detected' >&2
  exit 1
fi
if grep -nE 'OpenOptions|append\(|write_all\(|writeln!' "$root/src/logger.rs"; then
  printf '%s\n' 'local diagnostic file sink detected' >&2
  exit 1
fi

#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 1 ]; then
  echo "[error] Usage: $0 <binary-path>"
  exit 1
fi

BINARY_PATH="$1"
if [ ! -f "${BINARY_PATH}" ]; then
  echo "[error] Binary does not exist: ${BINARY_PATH}"
  exit 1
fi

case "${BINARY_PATH}" in
  *.exe) ;;
  *) chmod +x "${BINARY_PATH}" ;;
esac

echo "[run] Smoke test: ${BINARY_PATH} --help"
"${BINARY_PATH}" --help > /tmp/bootty_smoke_help.txt

echo "[ok] Smoke test passed"

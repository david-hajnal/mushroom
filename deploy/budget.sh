#!/usr/bin/env bash
# budget.sh — performance budget report for the Magic Mirror (Slice 7).
#
# Generates a report of the budgets called out in spec §10:
#   * Rust binary size (release, stripped) — reported, not asserted.
#   * Compressed static frontend (JS + CSS, excluding fonts) — target < 50 KB.
#   * Combined dashboard response — typically < 25 KB.
#   * Server warm idle RAM — < 50 MB desirable, < 100 MB acceptable
#     (measured where the platform exposes it; the browser/kiosk dominates
#     total memory and is out of scope for this server-side measurement).
#
# Usage:
#   bash deploy/budget.sh                 # build release + measure + write report
#   bash deploy/budget.sh --no-build      # skip the build (use existing binary)
#   BUDGET_REPORT=/path/to/report.md bash deploy/budget.sh
#
# The report is written to $BUDGET_REPORT (default: data/budget-report.md) and
# also printed to stdout. Exit code is 0 if the report is generated (regardless
# of whether the targets are met — the spec says *report* the sizes, and the
# RAM target is explicitly "avoid claiming a guaranteed target until
# benchmarked"); individual budget lines are marked PASS/FAIL for visibility.
#
# Cross-platform: works on Linux (the target) and macOS (the dev host).

set -euo pipefail

# --- Resolve paths ----------------------------------------------------------
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BUILD=1
if [[ "${1:-}" == "--no-build" ]]; then
  BUILD=0
fi

REPORT="${BUDGET_REPORT:-data/budget-report.md}"
BIN="target/release/magic-mirror"
STATIC_DIR="static"
# A unique port + temp snapshot so the measurement never collides with a
# running server or pollutes the repo.
PORT="${BUDGET_PORT:-3123}"
WORKDIR="$(mktemp -d)"
SNAPSHOT="$WORKDIR/snapshot.json"
SERVER_PID=""

cleanup() {
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORKDIR" 2>/dev/null || true
}
trap cleanup EXIT

# --- Helpers ----------------------------------------------------------------
# Human-readable byte count.
human() {
  local b="$1"
  if (( b >= 1048576 )); then
    awk -v b="$b" 'BEGIN{printf "%.1f MiB", b/1048576}'
  elif (( b >= 1024 )); then
    awk -v b="$b" 'BEGIN{printf "%.1f KiB", b/1024}'
  else
    printf "%d B" "$b"
  fi
}

# File size in bytes (portable across macOS / Linux).
file_size() {
  local f="$1"
  if stat -f%z "$f" >/dev/null 2>&1; then
    stat -f%z "$f"          # BSD (macOS)
  else
    stat -c%s "$f"          # GNU (Linux)
  fi
}

# Sum the byte sizes of files matching a glob (JS + CSS, excluding fonts).
static_asset_bytes() {
  local total=0 f
  while IFS= read -r -d '' f; do
    local s
    s="$(file_size "$f")"
    total=$(( total + s ))
  done < <(find "$STATIC_DIR" -type f \( -name '*.js' -o -name '*.css' \) -print0)
  printf '%s' "$total"
}

# Gzip-compressed size of the JS + CSS assets (the "compressed static
# frontend" budget, excluding fonts).
static_compressed_bytes() {
  local total=0 f
  while IFS= read -r -d '' f; do
    local s
    s="$(gzip -c "$f" | wc -c | tr -d '[:space:]')"
    total=$(( total + s ))
  done < <(find "$STATIC_DIR" -type f \( -name '*.js' -o -name '*.css' \) -print0)
  printf '%s' "$total"
}

# RSS of a PID in bytes (portable). Returns empty if unavailable.
rss_bytes() {
  local pid="$1" kb
  kb="$(ps -o rss= -p "$pid" 2>/dev/null | tr -d '[:space:]')" || return 1
  [[ -n "$kb" ]] || return 1
  printf '%s' "$(( kb * 1024 ))"
}

# Wait until the server answers /api/v1/health (up to ~10 s).
wait_ready() {
  local i
  for i in $(seq 1 50); do
    if curl -fsS "http://127.0.0.1:${PORT}/api/v1/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  return 1
}

# --- Build ------------------------------------------------------------------
if [[ "$BUILD" -eq 1 ]]; then
  echo "==> Building release binary" >&2
  cargo build --release
  # Strip symbols on platforms where `strip` is available (best-effort).
  if command -v strip >/dev/null 2>&1; then
    strip "$BIN" 2>/dev/null || true
  fi
fi

if [[ ! -f "$BIN" ]]; then
  echo "error: release binary not found at $BIN (run without --no-build)" >&2
  exit 1
fi

# --- Measure ----------------------------------------------------------------
echo "==> Measuring budgets" >&2

BIN_BYTES="$(file_size "$BIN")"
STATIC_RAW_BYTES="$(static_asset_bytes)"
STATIC_COMP_BYTES="$(static_compressed_bytes)"

# Start the server (defaults; no feeds configured => no network on idle) and
# measure warm idle RAM + the dashboard response size.
MEM_BYTES=""
DASH_BYTES=""
MEM_NOTE=""
DASH_NOTE=""

MIRROR_SNAPSHOT_PATH="$SNAPSHOT" \
  MIRROR_BIND="127.0.0.1:${PORT}" \
  MIRROR_STATIC_DIR="$ROOT/${STATIC_DIR}" \
  "$BIN" >/dev/null 2>&1 &
SERVER_PID=$!

if wait_ready; then
  # Let it settle to a warm idle state.
  sleep 1
  if MEM_BYTES="$(rss_bytes "$SERVER_PID")"; then
    :
  else
    MEM_NOTE="(RSS not measurable on this platform)"
  fi
  if DASH_BODY="$(curl -fsS "http://127.0.0.1:${PORT}/api/v1/dashboard" 2>/dev/null)"; then
    DASH_BYTES="$(printf '%s' "$DASH_BODY" | wc -c | tr -d '[:space:]')"
  else
    DASH_NOTE="(dashboard request failed)"
  fi
else
  MEM_NOTE="(server did not become ready)"
  DASH_NOTE="(server did not become ready)"
fi

# Stop the server.
if kill -0 "$SERVER_PID" 2>/dev/null; then
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
fi
SERVER_PID=""

# --- Budgets (targets from spec §10) ----------------------------------------
STATIC_TARGET=51200      # 50 KB compressed JS+CSS (excluding fonts)
DASH_TARGET=25600        # 25 KB dashboard response
MEM_DESIRED=52428800     # 50 MB warm idle RAM (desirable)
MEM_ACCEPTABLE=104857600 # 100 MB warm idle RAM (acceptable)

mark() { # mark <actual_bytes> <target_bytes>
  if [[ -n "$1" && -n "$2" ]]; then
    if (( $1 <= $2 )); then echo "PASS"; else echo "FAIL"; fi
  else
    echo "n/a"
  fi
}

STATIC_MARK="$(mark "$STATIC_COMP_BYTES" "$STATIC_TARGET")"
DASH_MARK="$(mark "$DASH_BYTES" "$DASH_TARGET")"
if [[ -n "$MEM_BYTES" ]]; then
  if (( MEM_BYTES <= MEM_DESIRED )); then MEM_MARK="PASS (<= 50 MB desirable)"
  elif (( MEM_BYTES <= MEM_ACCEPTABLE )); then MEM_MARK="PASS (<= 100 MB acceptable)"
  else MEM_MARK="FAIL (> 100 MB)"; fi
else
  MEM_MARK="n/a"
fi

# --- Report -----------------------------------------------------------------
mkdir -p "$(dirname "$REPORT")"
NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

{
  echo "# Magic Mirror — performance budget report"
  echo
  echo "Generated: ${NOW}"
  echo "Host: $(uname -s) $(uname -m)"
  echo
  echo "| Budget | Actual | Target | Status |"
  echo "|---|---|---|---|"
  echo "| Binary size (release, stripped) | $(human "$BIN_BYTES") | report only | — |"
  echo "| Static frontend (JS+CSS, excl. fonts), raw | $(human "$STATIC_RAW_BYTES") | — | — |"
  echo "| Static frontend (JS+CSS, excl. fonts), gzip | $(human "$STATIC_COMP_BYTES") | < 50 KB | ${STATIC_MARK} |"
  echo "| Dashboard response (/api/v1/dashboard) | $(human "${DASH_BYTES:-0}")${DASH_NOTE:+ ${DASH_NOTE}} | < 25 KB | ${DASH_MARK} |"
  echo "| Server warm idle RAM | $(human "${MEM_BYTES:-0}")${MEM_NOTE:+ ${MEM_NOTE}} | < 50 MB desirable / < 100 MB acceptable | ${MEM_MARK} |"
  echo
  echo "Notes:"
  echo "- Binary size is *reported*, not asserted (spec §10: \"report actual file size … rather than asserting a fixed threshold\")."
  echo "- The kiosk browser (Chromium) dominates total device memory and is out of scope for this server-side measurement (spec §10)."
  echo "- RAM is measured where the platform exposes it (ps RSS); the target is a guideline, not a guarantee."
} | tee "$REPORT"

echo
echo "==> Report written to ${REPORT}" >&2
exit 0

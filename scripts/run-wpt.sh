#!/usr/bin/env bash
set -euo pipefail

# Run a WPT pass against Obscura.
#
# Boots the WPT server, builds the runner, runs one benchmark profile, and pipes
# its JSON through triage. Any extra args are forwarded to wpt-runner.
#
# Env:
#   WPT_PROFILE      no-render (default) or render
#   OBSCURA_BIN      matching obscura binary (default: obscura on PATH)
#   OBSCURA_PORT     CDP port for the render profile (default: 9222)
#   OBSCURA_WORKERS  serve workers for the render profile (default: 4)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

OBSCURA_BIN="${OBSCURA_BIN:-obscura}"
WPT_PROFILE="${WPT_PROFILE:-no-render}"
OBSCURA_PORT="${OBSCURA_PORT:-9222}"
OBSCURA_WORKERS="${OBSCURA_WORKERS:-4}"
WPT_DIR="$SCRIPT_DIR/wpt"
RESULTS_DIR="$SCRIPT_DIR/results"
TARGET_DIR="${CARGO_TARGET_DIR:-$SCRIPT_DIR/target}"

if [ "$WPT_PROFILE" != "no-render" ] && [ "$WPT_PROFILE" != "render" ]; then
  echo "[run-wpt] error: WPT_PROFILE must be no-render or render" >&2
  exit 1
fi
if [[ "$TARGET_DIR" != /* ]]; then
  TARGET_DIR="$SCRIPT_DIR/$TARGET_DIR"
fi

WPT_URL="http://web-platform.test:8000/"
WPT_CA="${SSL_CERT_FILE:-$WPT_DIR/tools/certs/cacert.pem}"

mkdir -p "$RESULTS_DIR"

if [ ! -d "$WPT_DIR" ]; then
  echo "[run-wpt] error: no WPT checkout at $WPT_DIR. run scripts/setup-wpt.sh first." >&2
  exit 1
fi

WPT_PID=""
OBSCURA_PID=""
cleanup() {
  echo "[run-wpt] shutting down servers"
  if [ -n "$OBSCURA_PID" ] && kill -0 "$OBSCURA_PID" 2>/dev/null; then
    kill -- "-$OBSCURA_PID" 2>/dev/null || kill "$OBSCURA_PID" 2>/dev/null || true
  fi
  if [ -n "$WPT_PID" ] && kill -0 "$WPT_PID" 2>/dev/null; then
    kill -- "-$WPT_PID" 2>/dev/null || kill "$WPT_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

echo "[run-wpt] starting WPT server"
( cd "$WPT_DIR" && exec setsid ./wpt serve ) >"$RESULTS_DIR/wpt-serve.log" 2>&1 &
WPT_PID=$!

echo "[run-wpt] waiting for the WPT server to come up"
ready=0
for _ in $(seq 1 60); do
  if curl -fsS -o /dev/null "$WPT_URL" 2>/dev/null; then
    ready=1
    break
  fi
  if ! kill -0 "$WPT_PID" 2>/dev/null; then
    echo "[run-wpt] error: WPT server exited early (port already in use?). see $RESULTS_DIR/wpt-serve.log" >&2
    echo "[run-wpt] hint: leftover servers from a previous run hold the ports. clear them with: pkill -f serve.py" >&2
    exit 1
  fi
  sleep 1
done

if [ "$ready" != "1" ]; then
  echo "[run-wpt] error: WPT server did not become ready within 60s. see $RESULTS_DIR/wpt-serve.log" >&2
  exit 1
fi

echo "[run-wpt] WPT server is up"

if [ "$WPT_PROFILE" = "render" ]; then
  echo "[run-wpt] starting Obscura render server on port $OBSCURA_PORT"
  SSL_CERT_FILE="$WPT_CA" setsid "$OBSCURA_BIN" serve \
    --host 127.0.0.1 \
    --port "$OBSCURA_PORT" \
    --workers "$OBSCURA_WORKERS" \
    --allow-private-network \
    >"$RESULTS_DIR/obscura-render.log" 2>&1 &
  OBSCURA_PID=$!
  browser_ready=0
  for _ in $(seq 1 60); do
    if curl -fsS -o /dev/null "http://127.0.0.1:$OBSCURA_PORT/json/version" 2>/dev/null; then
      browser_ready=1
      break
    fi
    if ! kill -0 "$OBSCURA_PID" 2>/dev/null; then
      echo "[run-wpt] error: Obscura render server exited early. see $RESULTS_DIR/obscura-render.log" >&2
      exit 1
    fi
    sleep 1
  done
  if [ "$browser_ready" != "1" ]; then
    echo "[run-wpt] error: Obscura render server did not become ready within 60s" >&2
    exit 1
  fi
fi

echo "[run-wpt] building wpt-runner and triage"
( cd "$SCRIPT_DIR" && cargo build --release -p wpt-runner -p triage )

RUNNER="$TARGET_DIR/release/wpt-runner"
TRIAGE="$TARGET_DIR/release/triage"

STAMP="$(date +%s)"
RESULTS_JSON="$RESULTS_DIR/wpt-$WPT_PROFILE-$STAMP.json"
TRIAGE_MD="$RESULTS_DIR/triage-$WPT_PROFILE.md"

RUNNER_ARGS=(--profile "$WPT_PROFILE" --include-https --obscura-bin "$OBSCURA_BIN")
RUNNER_ARGS+=(--wpt-revision "$(git -C "$WPT_DIR" rev-parse HEAD)")
if [ "$WPT_PROFILE" = "render" ]; then
  RUNNER_ARGS+=(--backend cdp --browser-port "$OBSCURA_PORT")
else
  RUNNER_ARGS+=(--backend fetch)
fi

echo "[run-wpt] running $WPT_PROFILE profile (obscura bin: $OBSCURA_BIN)"
# The runner exits non-zero whenever any subtest fails, which is the normal case
# for a partial-conformance engine, so do not let `set -e` abort before triage.
# Only a missing or empty JSON file is a real failure.
SSL_CERT_FILE="$WPT_CA" "$RUNNER" --json "${RUNNER_ARGS[@]}" "$@" >"$RESULTS_JSON" || true
if [ ! -s "$RESULTS_JSON" ]; then
  echo "[run-wpt] error: runner produced no output." >&2
  exit 1
fi

echo "[run-wpt] generating triage report"
"$TRIAGE" <"$RESULTS_JSON" >"$TRIAGE_MD"

echo ""
echo "[run-wpt] done."
echo "[run-wpt] results json:  $RESULTS_JSON"
echo "[run-wpt] triage report: $TRIAGE_MD"

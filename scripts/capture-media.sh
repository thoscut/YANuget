#!/usr/bin/env bash
# Regenerate the screenshots and walkthrough GIFs used on the product page.
#
# Starts two throwaway servers — one seeded with a realistic feed, one empty so
# the first-run panel can be captured — drives a real browser against them, and
# assembles the frames into GIFs. Everything lands in .github/media/.
#
# The media is *generated*, never hand-edited: what it shows is what the code
# actually renders, and re-running after a UI change is how it stays true.
#
# Usage: scripts/capture-media.sh [output-dir]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-$REPO_ROOT/.github/media}"
WORK="$(mktemp -d)"
SEEDED_PORT="${SEEDED_PORT:-57811}"
EMPTY_PORT="${EMPTY_PORT:-57812}"
API_KEY="demo-key"

cleanup() {
    [ -n "${SEEDED_PID:-}" ] && kill "$SEEDED_PID" 2>/dev/null || true
    [ -n "${EMPTY_PID:-}" ] && kill "$EMPTY_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

echo "==> building the server"
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
BIN="$REPO_ROOT/target/release/yanuget"

# --- toolchain ---------------------------------------------------------------
# Pillow assembles the GIFs; Playwright drives the browser. Both are dev-only,
# so they live in this working directory rather than in the repo.
echo "==> preparing the capture toolchain"
python3 -m venv "$WORK/venv" >/dev/null
"$WORK/venv/bin/pip" install --quiet --no-cache-dir Pillow
PYTHON="$WORK/venv/bin/python"

if [ ! -d "$REPO_ROOT/scripts/media/node_modules/playwright" ]; then
    echo "==> installing playwright"
    npm install --prefix "$REPO_ROOT/scripts/media" --silent
    # PLAYWRIGHT_BROWSERS_PATH may already point at a shared browser install;
    # only fetch chromium when it is genuinely absent.
    if [ -z "${PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD:-}" ]; then
        npx --prefix "$REPO_ROOT/scripts/media" playwright install chromium
    fi
fi

# --- servers -----------------------------------------------------------------
# TLS off, because a self-signed certificate makes the browser show an interstitial
# instead of the gallery — and these captures are of the gallery.
start_server() {
    local dir="$1" port="$2" logfile="$3"
    mkdir -p "$dir"
    YANUGET_DATA_DIR="$dir" \
    YANUGET_PORT="$port" \
    YANUGET_API_KEY="$API_KEY" \
    YANUGET_TLS_ENABLED=false \
        "$BIN" >"$logfile" 2>&1 &
    echo $!
}

wait_for() {
    local port="$1"
    for _ in $(seq 1 60); do
        if curl -sf "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    echo "server on port $port never became healthy" >&2
    cat "$WORK/seeded.log" "$WORK/empty.log" 2>/dev/null >&2 || true
    return 1
}

echo "==> starting servers"
SEEDED_PID="$(start_server "$WORK/seeded" "$SEEDED_PORT" "$WORK/seeded.log")"
EMPTY_PID="$(start_server "$WORK/empty" "$EMPTY_PORT" "$WORK/empty.log")"
wait_for "$SEEDED_PORT"
wait_for "$EMPTY_PORT"

echo "==> seeding the feed"
"$PYTHON" "$REPO_ROOT/scripts/media/seed.py" \
    "http://127.0.0.1:$SEEDED_PORT" "$API_KEY"

echo "==> capturing"
node "$REPO_ROOT/scripts/media/capture.mjs" \
    "http://127.0.0.1:$SEEDED_PORT" \
    "http://127.0.0.1:$EMPTY_PORT" \
    "$WORK/shots"

echo "==> assembling GIFs"
"$PYTHON" "$REPO_ROOT/scripts/media/build_gif.py" \
    "$WORK/shots/frames/gallery" "$WORK/shots/gallery.gif" --width 900
"$PYTHON" "$REPO_ROOT/scripts/media/build_gif.py" \
    "$WORK/shots/frames/theme" "$WORK/shots/theme.gif" --width 780 --colors 96

mkdir -p "$OUT_DIR"
cp "$WORK/shots"/*.png "$WORK/shots"/*.gif "$OUT_DIR/"

echo
echo "==> written to $OUT_DIR"
ls -1sh "$OUT_DIR" | tail -n +2

#!/usr/bin/env bash
# cron-refresh.sh — Scan providers, refresh catalog, push to git.
# Used directly as a cron job or triggered via n8n Execute Command node.
# Example cron: 0 9 * * 1 /path/to/proxy-fire/scripts/cron-refresh.sh
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
SCAN_BIN="$REPO_DIR/bin/proxy-fire"
CATALOG_SRC="$HOME/.proxy-fire/catalog.json"
CATALOG_DEST="$REPO_DIR/configs/catalog.json"
LOG_FILE="$HOME/.proxy-fire/refresh.log"

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$LOG_FILE"; }

log "Starting catalog refresh"

# Build the all-in-one binary if missing or stale
if [ ! -f "$SCAN_BIN" ] || [ "$REPO_DIR/Cargo.toml" -nt "$SCAN_BIN" ]; then
  log "Building proxy-fire..."
  cargo build --release --bin proxy-fire --manifest-path "$REPO_DIR/Cargo.toml" && cp "$REPO_DIR/target/release/proxy-fire" "$SCAN_BIN"
fi

# 1. Scan all providers
log "Running provider scan..."
"$SCAN_BIN" scan update 2>&1 | tee -a "$LOG_FILE"
log "Scan complete"

# 2. LLM-powered diff refresh
log "Running LLM refresh..."
"$SCAN_BIN" scan refresh-llm 2>&1 | tee -a "$LOG_FILE"
log "LLM refresh complete"

# 3. Copy updated catalog into the repo and push
log "Pushing catalog to git..."
cp "$CATALOG_SRC" "$CATALOG_DEST"
cd "$REPO_DIR"
git add configs/catalog.json
if git diff --cached --quiet; then
  log "Catalog unchanged — nothing to push"
else
  ENTRY_COUNT=$(grep -o '"provider_id"' "$CATALOG_DEST" | wc -l)
  git commit -m "chore: auto-refresh catalog — ${ENTRY_COUNT} entries ($(date '+%Y-%m-%d'))"
  git push
  log "Catalog pushed ($ENTRY_COUNT entries)"
fi

log "Done"

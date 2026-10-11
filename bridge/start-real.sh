#!/usr/bin/env bash
set -euo pipefail

BRIDGE_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$BRIDGE_DIR"

if [[ ! -f .env ]]; then
  echo "Missing bridge/.env. Copy .env.example to .env and fill the VPS URL and shared secret." >&2
  exit 1
fi

if [[ ! -x .venv/bin/python ]]; then
  echo "Missing bridge/.venv. Create it as described in docs/DEPLOY_MACOS.md." >&2
  exit 1
fi

set -a
# shellcheck disable=SC1091
source .env
set +a

if [[ "${CONTROL_WS_URL:-}" == *"example.com"* ]]; then
  echo "CONTROL_WS_URL still contains the example domain." >&2
  exit 1
fi

if [[ -z "${BRIDGE_SHARED_SECRET:-}" || "${BRIDGE_SHARED_SECRET}" == replace-* ]]; then
  echo "BRIDGE_SHARED_SECRET is not configured." >&2
  exit 1
fi

# Build the health-check list from ASR_URLS (comma-separated transcription
# endpoints) when present; otherwise fall back to the single ASR_HEALTH_URL.
health_urls=()
if [[ -n "${ASR_URLS:-}" ]]; then
  IFS=',' read -r -a asr_url_list <<< "$ASR_URLS"
  for asr_url in "${asr_url_list[@]}"; do
    asr_url="$(echo "$asr_url" | xargs)"
    [[ -z "$asr_url" ]] && continue
    health_urls+=("${asr_url%%/v1/*}/health")
  done
else
  health_urls+=("${ASR_HEALTH_URL:-http://127.0.0.1:8080/health}")
fi

ASR_STARTUP_WAIT_SECONDS="${ASR_STARTUP_WAIT_SECONDS:-120}"
for health_url in "${health_urls[@]}"; do
  deadline=$((SECONDS + ASR_STARTUP_WAIT_SECONDS))
  until curl --fail --silent "$health_url" >/dev/null; do
    if (( SECONDS >= deadline )); then
      echo "ASR health check failed at $health_url after ${ASR_STARTUP_WAIT_SECONDS}s." >&2
      exit 1
    fi
    echo "Waiting for OminiX ASR at $health_url..." >&2
    sleep 2
  done
done

exec .venv/bin/python -m bridge.main --mode real

#!/usr/bin/env bash
# Shared hook helper: load local inband tokens and build signed curl auth arguments.

inband_load_auth() {
  local client="${1:-claude}"
  # Installs made before the rename keep their files under agent-bridge and use AGENT_BRIDGE_ names.
  local token_file="${INBAND_TOKENS_FILE:-${AGENT_BRIDGE_TOKENS_FILE:-$HOME/.local/share/mcp-servers/inband/tokens.env}}"
  [ -r "$token_file" ] || token_file="$HOME/.local/share/mcp-servers/agent-bridge/tokens.env"
  if [ -r "$token_file" ]; then
    set -a
    # shellcheck disable=SC1090
    . "$token_file"
    set +a
  fi

  local token_var legacy_var
  token_var="INBAND_${client^^}_TOKEN"
  legacy_var="AGENT_BRIDGE_${client^^}_TOKEN"
  local token="${!token_var:-${!legacy_var:-${INBAND_TOKEN:-${AGENT_BRIDGE_TOKEN:-}}}}"
  INBAND_AUTH_CLIENT="$client"
  INBAND_AUTH_TOKEN="$token"
  INBAND_CURL_AUTH=()
}

inband_sign_request() {
  local method="$1"
  local url="$2"
  local body="${3:-}"
  INBAND_CURL_AUTH=()
  if [ -z "${INBAND_AUTH_TOKEN:-}" ]; then
    return 0
  fi
  mapfile -d '' -t INBAND_CURL_AUTH < <(
    INBAND_AUTH_CLIENT="$INBAND_AUTH_CLIENT" \
    INBAND_AUTH_TOKEN="$INBAND_AUTH_TOKEN" \
    INBAND_AUTH_METHOD="$method" \
    INBAND_AUTH_URL="$url" \
    INBAND_AUTH_BODY="$body" \
    python3 - <<'PY'
import hashlib
import hmac
import os
import secrets
import sys
import time
from urllib.parse import urlsplit

client = os.environ["INBAND_AUTH_CLIENT"]
token = os.environ["INBAND_AUTH_TOKEN"]
method = os.environ["INBAND_AUTH_METHOD"].upper()
url = os.environ["INBAND_AUTH_URL"]
body = os.environ.get("INBAND_AUTH_BODY", "")

parts = urlsplit(url)
path = parts.path or "/"
if parts.query:
    path = f"{path}?{parts.query}"
timestamp = str(int(time.time() * 1000))
nonce = secrets.token_hex(16)
digest = hashlib.sha256(body.encode()).hexdigest()
payload = "\n".join([method, path, digest, timestamp, nonce, client])
signature = hmac.new(token.encode(), payload.encode(), hashlib.sha256).hexdigest()
headers = [
    "-H", f"x-inband-client: {client}",
    "-H", f"x-inband-timestamp: {timestamp}",
    "-H", f"x-inband-nonce: {nonce}",
    "-H", f"x-inband-signature: sha256={signature}",
]
sys.stdout.write("\0".join(headers) + "\0")
PY
  )
}

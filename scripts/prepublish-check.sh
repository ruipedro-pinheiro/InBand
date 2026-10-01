#!/usr/bin/env bash
set -euo pipefail

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "prepublish-check: run from inside the inband git repository" >&2
  exit 2
fi

forbidden=$(
  git ls-files -- \
    bridge.db bridge.db-shm bridge.db-wal \
    config.json tokens.env .env '*.env' \
    'backups/*' 'node_modules/*' 'dist/*' \
    '*.db' '*.db-shm' '*.db-wal' '*.log' '*.bak'
)

if [ -n "$forbidden" ]; then
  echo "prepublish-check: refusing to publish tracked local/secrets/runtime files:" >&2
  printf '%s\n' "$forbidden" >&2
  exit 1
fi

for private_file in config.json config.json.pre-auth.bak tokens.env bridge.db bridge.db-shm bridge.db-wal; do
  [ -e "$private_file" ] || continue
  mode=$(stat -c '%a' "$private_file")
  if (( (8#$mode & 077) != 0 )); then
    echo "prepublish-check: $private_file is too permissive ($mode), expected no group/other bits" >&2
    exit 1
  fi
done

echo "prepublish-check: ok"

#!/usr/bin/env bash
# Deploy pm-hub (crates/pm-hub) to Railway project `pm-hub`, service `hub`
# (AGT-1387), then check the public /health answers 200 from the new crate.
#
# Run from a clean checkout of the commit you mean to ship:
#   scripts/deploy-hub.sh
#
# Needs: railway CLI, curl, and the Studio keychain item
# `pm-hub.railway-project-token` (a Railway project token, scoped to the
# project's production environment). The token is read at run time, handed
# to `railway` through its environment only, and never printed.
#
# Railway builds the image remotely from crates/pm-hub/Dockerfile (see
# railway.toml) and only switches traffic once /health passes.

set -euo pipefail
set +x

readonly SERVICE=hub
readonly HEALTH_URL=https://hub-production-8a91.up.railway.app/health
readonly KEYCHAIN_ITEM=pm-hub.railway-project-token
readonly HEALTH_WAIT_SECS=180

die() {
  echo "deploy-hub: $*" >&2
  exit 1
}

for cmd in cargo railway curl security git; do
  command -v "$cmd" >/dev/null 2>&1 || die "missing required command: $cmd"
done

cd "$(git rev-parse --show-toplevel)"

# `railway up` uploads the working tree, so refuse to ship edits that are
# not in a commit.
if [[ -n "$(git status --porcelain)" ]]; then
  die "working tree is not clean; commit or stash first"
fi
echo "deploy-hub: deploying $(git rev-parse --short HEAD) to service $SERVICE"

# Fail fast locally before uploading anything.
cargo build --locked --release -p pm-hub

token="$(security find-generic-password -s "$KEYCHAIN_ITEM" -w)" ||
  die "could not read keychain item $KEYCHAIN_ITEM"
[[ -n "$token" ]] || die "keychain item $KEYCHAIN_ITEM is empty"

# Scoped to this one command: nothing else in the script sees the token.
RAILWAY_TOKEN="$token" railway up --service "$SERVICE" --environment production --ci
unset token

echo "deploy-hub: waiting for $HEALTH_URL"
body_file="$(mktemp)"
trap 'rm -f "$body_file"' EXIT
deadline=$((SECONDS + HEALTH_WAIT_SECS))
while :; do
  code="$(curl -sS -o "$body_file" -w '%{http_code}' --max-time 10 "$HEALTH_URL" || true)"
  # The hello-world this replaces may also answer 200; only the new
  # crate's body carries a schema_version.
  if [[ "$code" == 200 ]] && grep -q '"schema_version"' "$body_file"; then
    echo "deploy-hub: healthy: $(cat "$body_file")"
    exit 0
  fi
  if ((SECONDS >= deadline)); then
    die "/health did not return 200 from pm-hub within ${HEALTH_WAIT_SECS}s (last status: ${code:-none})"
  fi
  sleep 5
done

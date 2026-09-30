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
# Success means /health reports this commit's sha as "build", so a deploy
# that never switched traffic (old build still serving) fails the script.
#
# Railway builds the image remotely from crates/pm-hub/Dockerfile (selected by
# the service variable RAILWAY_DOCKERFILE_PATH; Railway ignores railway.toml)
# and, when the service's Healthcheck Path is set to /health, only switches
# traffic once it passes.

set -euo pipefail
set +x

readonly SERVICE=hub
readonly HEALTH_URL=https://hub-production-8a91.up.railway.app/health
readonly KEYCHAIN_ITEM=pm-hub.railway-project-token
readonly HEALTH_WAIT_SECS=600

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
sha="$(git rev-parse HEAD)"
echo "deploy-hub: deploying ${sha:0:12} to service $SERVICE"

# Fail fast locally before uploading anything.
cargo build --locked --release -p pm-hub

token="$(security find-generic-password -s "$KEYCHAIN_ITEM" -w)" ||
  die "could not read keychain item $KEYCHAIN_ITEM"
[[ -n "$token" ]] || die "keychain item $KEYCHAIN_ITEM is empty"

# Scoped to this one command: nothing else in the script sees the token.
# --detach, not --ci: streaming build logs fails with a project token
# ("Failed to retrieve build log") and aborted the script mid-build.
# The /health poll below (which covers the build time) is the success check.
# `railway up` uploads the working tree without .git and has no build-arg
# flag, so the sha travels as an (untracked, not gitignored) file in the
# upload: the Dockerfile reads .build-sha into PM_HUB_BUILD_SHA, and
# pm-hub reports it as "build" in /health. Removed again on exit.
body_file="$(mktemp)"
trap 'rm -f .build-sha "$body_file"' EXIT
printf '%s\n' "$sha" >.build-sha
RAILWAY_TOKEN="$token" railway up --service "$SERVICE" --environment production --detach
unset token
rm -f .build-sha

echo "deploy-hub: waiting for $HEALTH_URL"
deadline=$((SECONDS + HEALTH_WAIT_SECS))
while :; do
  code="$(curl -sS -o "$body_file" -w '%{http_code}' --max-time 10 "$HEALTH_URL" || true)"
  # Wait for *this* build, not just any healthy hub: the previous
  # deployment keeps answering 200 until Railway switches traffic, so the
  # body must carry the sha we just shipped (and a schema_version, which
  # the hello-world this replaced never had).
  if [[ "$code" == 200 ]] && grep -q '"schema_version"' "$body_file" &&
    grep -q "\"build\":\"$sha\"" "$body_file"; then
    echo "deploy-hub: healthy, serving ${sha:0:12}: $(cat "$body_file")"
    exit 0
  fi
  if ((SECONDS >= deadline)); then
    die "/health did not return 200 from pm-hub within ${HEALTH_WAIT_SECS}s (last status: ${code:-none}; wanted build ${sha:0:12}, last body: $(head -c 300 "$body_file"))"
  fi
  sleep 5
done

#!/usr/bin/env bash
# Refresh the snapshot of t3code's model manifest that the binary is built with.
#
#   scripts/sync-models.sh            # fetch, check it reads, write crates/neosh-provider/t3code-models.json
#   scripts/sync-models.sh --check    # say whether the snapshot is behind, write nothing
#
# A running workspace fetches the same file every six hours (`models.update`), so this is not what
# keeps anybody current — it is what a machine that is offline, firewalled or brand new starts with,
# and it is run by `scripts/bump.sh` so every release carries the list as of the day it was cut.
# What comes back is checked by the very parser that will read it (`neosh_provider::t3code`) before
# it replaces anything: their file owes us no stability, and a snapshot we cannot read is a release
# whose `^P` shows only what `models.json` knew.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

url="https://raw.githubusercontent.com/pingdotgg/t3code/main/apps/server/src/provider/model-manifest.json"
snapshot=crates/neosh-provider/t3code-models.json
check=false
[ "${1:-}" = "--check" ] && check=true

tmp=$(mktemp)
trap 'rm -f "$tmp" "$snapshot.was"' EXIT
curl -fsSL --max-time 30 "$url" -o "$tmp"

if cmp -s "$tmp" "$snapshot"; then
  echo "t3code model manifest: up to date"
  exit 0
fi
stamp() { grep -m1 -o '"updatedAt": *"[^"]*"' "$1" | sed 's/.*: *"//; s/"$//'; }
echo "t3code model manifest: $(stamp "$snapshot") → $(stamp "$tmp")"
if $check; then
  exit 1
fi

cp "$snapshot" "$snapshot.was"
cp "$tmp" "$snapshot"
# The bundled-snapshot tests parse it and lay it over models.json, which is the whole question.
if ! cargo test -q -p neosh-provider --lib t3code >/dev/null 2>&1; then
  mv "$snapshot.was" "$snapshot"
  echo "error: the new manifest does not read — kept the old snapshot." >&2
  echo "       cargo test -p neosh-provider --lib t3code   says why" >&2
  exit 1
fi
echo "written: $snapshot"

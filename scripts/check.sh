#!/usr/bin/env bash
#
# Everything CI runs. Keep this the single source of truth so "works locally" means something.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

step "cargo test --workspace"
cargo test --workspace

# The plugin API's wire types are generated from Rust by ts-rs and committed. If a Rust type changed
# without the generated TypeScript being regenerated, the plugin ecosystem is now compiling against
# a lie — so that is a build failure, not a warning.
step "TypeScript bindings are in sync with the Rust types"
cargo test -p neosh-proto --quiet >/dev/null
if ! git diff --exit-code -- plugins/api/src/generated; then
  echo
  echo "error: generated TypeScript is out of date with the Rust types."
  echo "       run 'cargo test -p neosh-proto' and commit plugins/api/src/generated."
  exit 1
fi

# The generated types are the wire truth; the hand-written ergonomic layer sits on top of them.
# This is what proves the two agree.
step "plugin API type-checks against the generated types"
(cd plugins/api && npm install --silent --no-audit --no-fund && npx tsc --noEmit)

step "the example plugin type-checks against the published API"
(cd examples/hello-plugin && ../../plugins/api/node_modules/.bin/tsc --noEmit)

# The bundled plugins are the proof that the API is sufficient: the sidebar and the switchers are
# written against exactly what a third party has. If one of them stops type-checking, either the API
# changed under it or it reached for something that is not public.
step "the bundled plugins type-check against the published API"
(cd plugins/builtin && ../api/node_modules/.bin/tsc --noEmit)

# `neosh init` writes a starter config *and* the types it is checked against, both emitted from the
# binary. If the template drifts from the API, every new user's first experience is a type error.
step "the scaffolded config type-checks against its own emitted types"
scaffold="$(mktemp -d)"
trap 'rm -rf "$scaffold"' EXIT
cargo run --quiet -p neosh -- --config-dir "$scaffold" init >/dev/null
./plugins/api/node_modules/.bin/tsc --noEmit --project "$scaffold/tsconfig.json"

printf '\n\033[32mall checks passed\033[0m\n'

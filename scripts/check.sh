#!/usr/bin/env bash
#
# Everything CI runs. Keep this the single source of truth so "works locally" means something.
#
# Run with no argument and it does all of it, which is what a contributor wants. CI runs the stages
# below on a runner each, in parallel, so a pull request waits for the slowest of them rather than
# the sum — and every stage compiles only what it tests, which is why nothing builds the binary to
# test the crates under it. The split is here rather than in the workflow so the workflow still has
# exactly one thing to call, and so `./scripts/check.sh` locally still means all of it.
#
#   crates          every crate below the binary: unit and integration tests, doctests, the ts-rs
#                   drift check, and what `cargo package` would ship
#   binary [N/M]    the neosh binary: its unit tests, every suite that drives it except the one
#                   below, and the config it scaffolds. N/M runs one slice of the suites
#   screen [N/M]    tests/builtin_plugins.rs — one whole neosh booted per test, and the longest
#                   thing here by a factor of three. N/M runs one slice of it
#   web             the TypeScript: the plugin API, the example, the bundled plugins, what each of
#                   them publishes, and the bits git stores on the scripts a workflow runs
#
# Tests run under `cargo nextest` when it is installed (https://nexte.st — one process per test,
# and the retry policy CI uses is `.config/nextest.toml`) and under `cargo test` when it is not.
# Slicing a stage needs nextest.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() { echo "usage: $0 [all | crates | binary [N/M] | screen [N/M] | web]" >&2; exit 2; }
WANT="${1:-all}"
SLICE="${2:-}"
case "$WANT" in
  all | crates | web) [ -z "$SLICE" ] || usage ;;
  binary | screen) ;;
  *) usage ;;
esac
want() { [ "$1" = "$WANT" ] || [ "$WANT" = all ]; }

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

# ts-rs writes the bindings wherever this names, and an exported value beats `.cargo/config.toml`
# — so in a git worktree an inherited one lands the export in another checkout, and the drift check
# then compares files nothing wrote to. Pinned to *this* checkout before any cargo runs.
export TS_RS_EXPORT_DIR="$PWD/plugins/api/src/generated"

TARGET="${CARGO_TARGET_DIR:-target}"

have_nextest() { cargo nextest --version >/dev/null 2>&1; }

# The pty suites start a whole workspace per test, and a machine starting one per core starves
# them all: a different test timed out on every run. Half the cores is the rate they stay honest
# at, on a laptop and on a 4-vCPU runner alike. NEOSH_TEST_THREADS overrides it.
cores() { nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 2; }
HEAVY="${NEOSH_TEST_THREADS:-$(( $(cores) / 2 ))}"
[ "$HEAVY" -ge 1 ] || HEAVY=1

# tests THREADS <cargo args> — the same package selection under either runner.
tests() {
  local threads="$1"; shift
  if have_nextest; then
    cargo nextest run --test-threads "$threads" "$@"
  else
    cargo test "$@" -- --test-threads "$threads"
  fi
}

# sliced N/M <cargo args> — one slice of a stage, or all of it when N/M is empty. `cargo test`
# has no partitioning, so a slice is the one thing here that needs nextest.
sliced() {
  local slice="$1"; shift
  if [ -z "$slice" ]; then
    tests "$HEAVY" "$@"
  elif have_nextest; then
    cargo nextest run --test-threads "$HEAVY" --partition "hash:$slice" "$@"
  else
    echo "error: running a slice ($slice) needs cargo-nextest — https://nexte.st" >&2
    exit 2
  fi
}

# The node checks run `tsc` out of `plugins/api/node_modules`, so every stage with a `tsc` in it
# needs this first.
npm_ready() {
  step "plugin API type-checks against the generated types"
  (cd plugins/api && npm install --silent --no-audit --no-fund && npx tsc --noEmit)
}

if want crates; then
  step "the crates below the binary"
  # Everything but `neosh`, which is the crate that costs half the build and is tested by the two
  # stages after this one. Light tests, so every core.
  tests "$(cores)" --workspace --exclude neosh

  # nextest does not run doctests; `cargo test` already did.
  if have_nextest; then
    step "the crates below the binary: doctests"
    cargo test --doc --workspace --exclude neosh
  fi

  # The plugin API's wire types are generated from Rust by ts-rs and committed. If a Rust type
  # changed without the generated TypeScript being regenerated, the plugin ecosystem is now
  # compiling against a lie — so that is a build failure, not a warning. The export is a test in
  # `neosh-proto` and has just run, into the directory pinned at the top.
  step "TypeScript bindings are in sync with the Rust types"
  if ! git diff --exit-code -- plugins/api/src/generated; then
    echo
    echo "error: generated TypeScript is out of date with the Rust types."
    echo "       run 'TS_RS_EXPORT_DIR=\"$PWD/plugins/api/src/generated\" cargo test -p neosh-proto'"
    exit 1
  fi

  # The plugin tree is embedded with `include_dir!`, which reads the filesystem — so it embeds
  # whatever is next to the checkout and says nothing about what a *published* crate would carry.
  # `neosh-script` used to reach two directories up for it: that built here and packaged to
  # nothing, and the crate would have gone to crates.io with no plugins and no API source in it.
  # `plugins/` is a crate of its own now so the tree sits inside a package root, and this is what
  # proves it still does. A file list, not a build — the real proof is `cargo publish --dry-run` at
  # release time, and that one costs a quarter of an hour.
  step "the published crate would actually contain the plugins"
  packaged="$(cargo package --list --allow-dirty -p neosh-plugins)"
  for required in "api/src/index.ts" "builtin/sidebar/plugin.toml" "api/src/generated"; do
    if ! grep -q "$required" <<<"$packaged"; then
      echo "error: '$required' is missing from the packaged neosh-plugins crate." >&2
      echo "       the binary would build here and ship empty from crates.io." >&2
      exit 1
    fi
  done
  # The other half: `include` globs override .gitignore, so junk gets published rather than skipped.
  if grep -qE '(^|/)(\._|node_modules/)' <<<"$packaged"; then
    echo "error: the packaged neosh-plugins crate contains node_modules or macOS ._ sidecars." >&2
    exit 1
  fi
fi

if want binary; then
  step "the neosh binary: unit tests, and every suite that drives it but one${SLICE:+ — slice $SLICE}"
  # Named one by one rather than `--tests`, so that the one suite the `screen` stage owns is not
  # compiled, linked and run here as well. A new file under tests/ is picked up by the glob.
  suites=()
  for f in crates/neosh/tests/*.rs; do
    name="$(basename "$f" .rs)"
    [ "$name" = builtin_plugins ] || suites+=(--test "$name")
  done
  sliced "$SLICE" -p neosh --bins "${suites[@]}"

  # `neosh init` writes a starter config *and* the types it is checked against, both emitted from
  # the binary. If the template drifts from the API, every new user's first experience is a type
  # error. The binary is the one the suites above just drove — `cargo run` would build it again
  # under the dev profile, which was two minutes of every run spent relinking the largest crate
  # for no new information.
  npm_ready
  step "the scaffolded config type-checks against its own emitted types"
  scaffold="$(mktemp -d)"
  trap 'rm -rf "$scaffold"' EXIT
  "$TARGET/debug/neosh" --config-dir "$scaffold" init >/dev/null
  ./plugins/api/node_modules/.bin/tsc --noEmit --project "$scaffold/tsconfig.json"
fi

if want screen; then
  step "the bundled plugins, on screen, through the binary${SLICE:+ — slice $SLICE}"
  sliced "$SLICE" -p neosh --test builtin_plugins
fi

if want web; then
  # `all` has already done this on its way through the binary stage; doing it twice is a wasted
  # `npm install` rather than a wrong answer, so it is guarded rather than reordered.
  if [ "$WANT" = web ]; then
    npm_ready
  fi

  step "the example plugin type-checks against the published API"
  (cd examples/hello-plugin && ../../plugins/api/node_modules/.bin/tsc --noEmit)

  # The bundled plugins are the proof that the API is sufficient: the sidebar and the switchers are
  # written against exactly what a third party has. If one of them stops type-checking, either the
  # API changed under it or it reached for something that is not public.
  step "the bundled plugins type-check against the published API"
  (cd plugins/builtin && ../api/node_modules/.bin/tsc --noEmit)

  # Every bundled plugin is an ordinary npm package, and what npm ships is the `files` list rather
  # than the directory. `@neosh/model` is two files — `main.ts` imports `./options.ts` — and a list
  # naming only `main.ts` publishes a package that cannot start, which nothing here would have
  # caught because the *binary* embeds the whole directory regardless. The other half is that a
  # `*.ts` glob matches `._main.ts` on a non-HFS+ volume, so the negation earns its place too.
  step "every bundled plugin publishes all of itself and none of the junk"
  for dir in plugins/builtin/*/; do
    name=$(basename "$dir")
    on_disk=$(ls "$dir" | grep -E '\.ts$' | sort | tr '\n' ' ')
    packed=$( (cd "$dir" && npm pack --dry-run 2>&1) \
      | grep -E '^npm notice [0-9]' | awk '{print $4}' | grep -E '\.ts$' | sort | tr '\n' ' ' || true)
    if [ "$on_disk" != "$packed" ]; then
      echo "error: @neosh/$name would publish the wrong files." >&2
      echo "       on disk: $on_disk" >&2
      echo "       packed : $packed" >&2
      echo "       fix the \"files\" list in $dir/package.json" >&2
      exit 1
    fi
  done

  # A script a workflow invokes as `./script` has to be executable *in git*. This repository is
  # often checked out on a volume that cannot store the bit, so `chmod +x` here changes nothing
  # that gets committed — and the failure is a release job exiting 126 on a runner, an hour after
  # anyone could have noticed.
  step "the scripts a workflow runs are executable"
  for script in scripts/*.sh; do
    mode=$(git ls-files -s "$script" | cut -d' ' -f1)
    if [ "$mode" != "100755" ]; then
      echo "error: $script is $mode in git, not 100755." >&2
      echo "       fix with: git update-index --chmod=+x $script" >&2
      exit 1
    fi
  done
fi

printf '\n\033[32mall checks passed\033[0m\n'

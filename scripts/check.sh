#!/usr/bin/env bash
#
# Everything CI runs. Keep this the single source of truth so "works locally" means something.
#
# Run with no argument and it does all of it, which is what a contributor wants. CI passes `rust`
# or `web` to run one half on each of two runners: the node checks need only committed files, so
# making them queue behind a nine-minute cargo build was five minutes of wall clock spent on
# nothing. The split is here rather than in the workflow so the workflow still has exactly one
# thing to call, and so `./scripts/check.sh` locally still means all of it.
set -euo pipefail
cd "$(dirname "$0")/.."

want() { [ "${1:-all}" = "$WANT" ] || [ "$WANT" = all ]; }
WANT="${1:-all}"
case "$WANT" in
  all | rust | web) ;;
  *) echo "usage: $0 [all|rust|web]" >&2; exit 2 ;;
esac

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

# The node checks run `tsc` out of `plugins/api/node_modules`, so both halves need it installed —
# it is the one thing neither half can do without.
npm_ready() {
  step "plugin API type-checks against the generated types"
  (cd plugins/api && npm install --silent --no-audit --no-fund && npx tsc --noEmit)
}

if want rust; then
  step "cargo test --workspace"
  cargo test --workspace

  # The plugin API's wire types are generated from Rust by ts-rs and committed. If a Rust type
  # changed without the generated TypeScript being regenerated, the plugin ecosystem is now
  # compiling against a lie — so that is a build failure, not a warning.
  step "TypeScript bindings are in sync with the Rust types"
  # Pinned to *this* checkout. ts-rs reads the directory from the environment, and an exported
  # `TS_RS_EXPORT_DIR` beats `.cargo/config.toml` — so in a git worktree the export lands in
  # whichever checkout that variable happens to name, and the diff below then compares files
  # nothing wrote to. A drift check that passes because it looked at the wrong tree is worse than
  # no drift check.
  TS_RS_EXPORT_DIR="$PWD/plugins/api/src/generated" cargo test -p neosh-proto --quiet >/dev/null
  if ! git diff --exit-code -- plugins/api/src/generated; then
    echo
    echo "error: generated TypeScript is out of date with the Rust types."
    echo "       run 'TS_RS_EXPORT_DIR=\"$PWD/plugins/api/src/generated\" cargo test -p neosh-proto'"
    exit 1
  fi

  # `neosh init` writes a starter config *and* the types it is checked against, both emitted from
  # the binary. If the template drifts from the API, every new user's first experience is a type
  # error. This one needs both halves — a cargo build and a `tsc` — which is why it is on the rust
  # runner and why that runner installs npm too.
  npm_ready
  step "the scaffolded config type-checks against its own emitted types"
  scaffold="$(mktemp -d)"
  trap 'rm -rf "$scaffold"' EXIT
  cargo run --quiet -p neosh -- --config-dir "$scaffold" init >/dev/null
  ./plugins/api/node_modules/.bin/tsc --noEmit --project "$scaffold/tsconfig.json"

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

if want web; then
  # `all` has already done this on its way through the rust half; doing it twice is a wasted
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

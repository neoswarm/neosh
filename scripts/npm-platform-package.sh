#!/usr/bin/env bash
#
# Build one platform package — `@neosh/cli-<os>-<cpu>` — into a directory ready for `npm publish`.
#
#   scripts/npm-platform-package.sh <rust-target> <version> <out-dir> [path/to/neosh]
#
# The binary is optional: without one this writes the package with an empty `bin/`, which is how
# the four names get reserved before any release exists. npm will not attach a trusted publisher to
# a package that does not exist, and these four are published by a workflow, so they have to be
# created before that workflow can ever run tokenlessly.
set -euo pipefail

target="${1:?usage: npm-platform-package.sh <rust-target> <version> <out-dir> [binary]}"
version="${2:?missing version}"
out="${3:?missing out dir}"
binary="${4:-}"

# The `os` and `cpu` fields are the whole mechanism: npm evaluates them before unpacking, installs
# the one package that matches and silently skips the other three. Getting these wrong does not
# error, it just installs four binaries or none.
case "$target" in
  aarch64-apple-darwin)      pkg_os=darwin; pkg_cpu=arm64; human="macOS on Apple silicon" ;;
  x86_64-apple-darwin)       pkg_os=darwin; pkg_cpu=x64;   human="macOS on Intel" ;;
  x86_64-unknown-linux-gnu)  pkg_os=linux;  pkg_cpu=x64;   human="Linux on x86-64" ;;
  aarch64-unknown-linux-gnu) pkg_os=linux;  pkg_cpu=arm64; human="Linux on ARM64" ;;
  *) echo "npm-platform-package.sh: unknown target $target" >&2; exit 1 ;;
esac

name="@neosh/cli-${pkg_os}-${pkg_cpu}"

rm -rf "$out"
mkdir -p "$out/bin"

cat > "$out/package.json" <<JSON
{
  "name": "$name",
  "version": "$version",
  "description": "The neosh binary for $human. Installed for you by the \`neosh\` package.",
  "license": "MIT",
  "homepage": "https://github.com/neoswarm/neosh",
  "repository": {
    "type": "git",
    "url": "git+https://github.com/neoswarm/neosh.git",
    "directory": "npm/neosh"
  },
  "os": ["$pkg_os"],
  "cpu": ["$pkg_cpu"],
  "files": ["bin", "README.md", "!._*"]
}
JSON

cat > "$out/README.md" <<MD
# $name

The neosh binary for $human.

You do not install this directly. It is an optional dependency of
[\`neosh\`](https://www.npmjs.com/package/neosh), and npm picks the one package matching your
platform out of the four.

\`\`\`sh
npm install -g neosh
\`\`\`
MD

if [ -n "$binary" ]; then
  [ -f "$binary" ] || { echo "npm-platform-package.sh: no binary at $binary" >&2; exit 1; }
  install -m 755 "$binary" "$out/bin/neosh"
else
  # An empty package still has to have the directory its `files` list names, or npm publishes
  # nothing under `bin/` and the placeholder is not even shaped like the real thing.
  : > "$out/bin/.gitkeep"
fi

echo "$name@$version -> $out"

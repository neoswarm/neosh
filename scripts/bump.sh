#!/usr/bin/env bash
# Bump the version for a release: the crates and the binary together, and each npm package only if
# it changed.
#
#   scripts/bump.sh                 # patch: 0.4.11 → 0.4.12, and a patch on every changed package
#   scripts/bump.sh minor           # 0.4.11 → 0.5.0 — 0.x breaks at the minor; see docs/releasing.md
#   scripts/bump.sh 0.6.0           # an exact number for the crates; changed packages take a patch
#   scripts/bump.sh --dry-run       # say what would move, write nothing
#   scripts/bump.sh --tag           # …then commit it as `release: vX.Y.Z` and make the annotated tag
#   scripts/bump.sh --push          # …and push it, which *is* the release (tag.yml and friends)
#
# Two kinds of thing carry a version, and they move differently.
#
# **One number for the crates and the binary.** The eleven crates, the `neosh` launcher and its
# four `@neosh/cli-*` packages. The binary changes every release, `neosh-plugins` changes whenever
# any plugin does, and `neosh-proto` — which nearly every crate depends on — changes in most
# releases; under 0.x a minor bump of it forces a republish of everything that pins it. Keeping
# eleven numbers apart would save a crate or two now and then at the cost of eleven decisions per
# release, so they stay in lockstep: `[workspace.package] version`, the ten internal pins in
# `[workspace.dependencies]`, and `npm/neosh/package.json` with its four optional dependencies.
#
# **A number of its own for every npm package** — `@neosh/api` and `@neosh/<plugin>`. They depend on
# nothing, not on each other and not on the API package, and the binary does not read them: it
# embeds the plugin tree at build time. So a package moves only when its directory changed since the
# last release (`--since`, default the most recent `v*` tag), and `publish-npm.yml` skips every
# version npm already has — an unchanged package is simply not published again.
#
# The failure that policy makes possible is a package whose directory changed and whose version did
# not: npm already has that number, the workflow skips it, and the change never reaches anybody.
# `--verify` is the check for exactly that and nothing else, and `tag.yml` runs it on every release
# tag against the tag before it.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

usage() { sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

level="patch"
packages=""
since=""
all=false
dry=false
tag=false
push=false
verify=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    patch | minor | major) level="$1" ;;
    [0-9]*.[0-9]*.[0-9]*) level="$1" ;;
    --packages) packages="${2:?--packages needs patch, minor or major}"; shift ;;
    --since) since="${2:?--since needs a git ref}"; shift ;;
    --all) all=true ;;
    --dry-run | -n) dry=true ;;
    --tag) tag=true ;;
    --push) tag=true; push=true ;;
    --verify) verify=true ;;
    -h | --help) usage 0 ;;
    *) echo "bump: unknown argument '$1'" >&2; usage 1 ;;
  esac
  shift
done
case "$packages" in "" | patch | minor | major) ;; *) echo "bump: --packages is patch, minor or major" >&2; exit 1 ;; esac

# ---- helpers --------------------------------------------------------------------------------------

# `bumped 0.4.11 minor` → 0.5.0. A pre-release suffix is dropped: bumping `0.5.0-rc.1` by a patch is
# `0.5.1`, and releasing the `0.5.0` it was a candidate for is `scripts/bump.sh 0.5.0`.
bumped() {
  local v="${1%%-*}" major minor patch
  IFS=. read -r major minor patch <<<"$v"
  case "$2" in
    major) echo "$((major + 1)).0.0" ;;
    minor) echo "$major.$((minor + 1)).0" ;;
    patch) echo "$major.$minor.$((patch + 1))" ;;
    *) echo "$2" ;;
  esac
}

# Strictly newer, by semver precedence on the numbers.
newer() {
  [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$1" ]
}

json_version() { node -p "require('./$1').version"; }

# The version a package.json had at a ref, or nothing if the file was not there.
version_at() {
  { git show "$1:$2" 2>/dev/null || true; } | node -e \
    'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{console.log(JSON.parse(s).version)}catch{}})'
}

# Set `version` in a package.json — and, for the launcher, the four binary packages it pins — keeping
# the file's own layout: two spaces and a trailing newline, which is how npm writes every one of them.
set_json_version() {
  node -e '
    const fs = require("fs");
    const [file, version, pins] = process.argv.slice(1);
    const pkg = JSON.parse(fs.readFileSync(file, "utf8"));
    pkg.version = version;
    if (pins === "pins") {
      for (const dep of Object.keys(pkg.optionalDependencies ?? {})) {
        if (dep.startsWith("@neosh/cli-")) pkg.optionalDependencies[dep] = version;
      }
    }
    fs.writeFileSync(file, JSON.stringify(pkg, null, 2) + "\n");
  ' "$1" "$2" "${3:-}"
}

# The lockfile carries the package's own version twice; npm rewrites it on the next install anyway,
# and a lockfile a release behind is a diff nobody made on somebody's next `npm install`.
set_lock_version() {
  [ -f "$1" ] || return 0
  node -e '
    const fs = require("fs");
    const [file, version] = process.argv.slice(1);
    const lock = JSON.parse(fs.readFileSync(file, "utf8"));
    lock.version = version;
    if (lock.packages && lock.packages[""]) lock.packages[""].version = version;
    fs.writeFileSync(file, JSON.stringify(lock, null, 2) + "\n");
  ' "$1" "$2"
}

workspace_version() {
  awk '/^\[workspace\.package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{gsub(/.*= *"|".*/,"");print;exit}' Cargo.toml
}

# Every npm package whose version is its own, as directories.
package_dirs() {
  echo plugins/api
  for d in plugins/builtin/*/; do
    d="${d%/}"
    if [ -f "$d/package.json" ]; then echo "$d"; fi
  done
}

# Whether a package's directory differs from `$base`: committed or not, and files git has not seen
# yet count — a new file in a plugin is a change to it.
changed_since() {
  local dir="$1" to="${2:-}"
  if [ -n "$to" ]; then
    ! git diff --quiet "$base" "$to" -- "$dir"
  else
    ! git diff --quiet "$base" -- "$dir" || [ -n "$(git ls-files --others --exclude-standard -- "$dir")" ]
  fi
}

# ---- what to compare against ------------------------------------------------------------------------

if [ -z "$since" ]; then
  if $verify; then
    # On a release tag, the one before it. `HEAD^` so that a tag on HEAD does not find itself.
    since=$(git describe --tags --abbrev=0 --match 'v[0-9]*' HEAD^ 2>/dev/null || true)
  else
    since=$(git describe --tags --abbrev=0 --match 'v[0-9]*' 2>/dev/null || true)
  fi
fi
if [ -z "$since" ]; then
  if $verify; then
    echo "bump --verify: no earlier v* tag, so nothing to compare against — nothing to check"
    exit 0
  fi
  echo "bump: no v* tag to compare against — every package counts as changed" >&2
  all=true
  base=""
else
  base=$(git rev-parse --verify --quiet "$since^{commit}") || {
    echo "bump: '$since' is not a commit here (fetch the tags? git fetch --tags)" >&2
    exit 1
  }
fi

# ---- --verify: every package that changed carries a new version ---------------------------------

if $verify; then
  bad=""
  for dir in $(package_dirs); do
    name=$(node -p "require('./$dir/package.json').name")
    now=$(json_version "$dir/package.json")
    was=$(version_at "$base" "$dir/package.json")
    [ -z "$was" ] && continue # new since then: its first version is whatever it says
    if changed_since "$dir" HEAD && ! newer "$now" "$was"; then
      echo "::error::$name changed since $since but is still $now — npm already has it, so the change would never be published"
      bad="$bad $name"
    fi
  done
  if [ -n "$bad" ]; then
    echo "Bump them with scripts/bump.sh (it moves exactly the packages that changed), commit, and re-tag."
    exit 1
  fi
  echo "every npm package that changed since $since carries a new version"
  exit 0
fi

# ---- plan -----------------------------------------------------------------------------------------

current=$(workspace_version)
[ -n "$current" ] || { echo "bump: no version in [workspace.package]" >&2; exit 1; }
next=$(bumped "$current" "$level")
if ! [[ "$next" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "bump: '$next' is not a version" >&2
  exit 1
fi
if ! newer "$next" "$current"; then
  echo "bump: $next is not newer than $current" >&2
  exit 1
fi
# How far a changed package moves: what was asked for, unless that was an exact number — which is a
# number for the crates and says nothing about a plugin that is on a numbering of its own.
if [ -z "$packages" ]; then
  case "$level" in patch | minor | major) packages="$level" ;; *) packages="patch" ;; esac
fi

if $tag; then
  if [ -n "$(git status --porcelain)" ]; then
    echo "bump: --tag commits only the version bump, and the tree has other changes in it." >&2
    echo "      Commit or stash them first, so the tag is on what you tested." >&2
    exit 1
  fi
  if git rev-parse --verify --quiet "refs/tags/v$next" >/dev/null; then
    echo "bump: tag v$next already exists" >&2
    exit 1
  fi
fi

printf '\n  %-22s %s → %s\n' "crates + binaries" "$current" "$next"
moves=()
firsts=()
for dir in $(package_dirs); do
  name=$(node -p "require('./$dir/package.json').name")
  now=$(json_version "$dir/package.json")
  if [ -n "$base" ] && [ -z "$(version_at "$base" "$dir/package.json")" ]; then
    # Never released: its first version is the one it already says, and npm has never seen the
    # name, so OIDC cannot publish it — a person does, once. Said here, where it will be read,
    # rather than an hour later as a red job.
    printf '  %-22s %s   new — first publish is by hand, see docs/releasing.md\n' "$name" "$now"
    firsts+=("$name")
  elif $all || changed_since "$dir"; then
    to=$(bumped "$now" "$packages")
    printf '  %-22s %s → %s\n' "$name" "$now" "$to"
    moves+=("$dir=$to")
  else
    printf '  %-22s %s   unchanged since %s\n' "$name" "$now" "$since"
  fi
done
echo

if $dry; then
  echo "dry run — nothing written"
  exit 0
fi

# ---- write ----------------------------------------------------------------------------------------

# `[workspace.package] version`, and the ten internal pins beside a `path` — both halves, or the
# workspace stops resolving (docs/releasing.md, "both halves of the root manifest").
perl -0pi -e '
  s/(\[workspace\.package\][^\[]*?\nversion\s*=\s*")\Q'"$current"'\E(")/${1}'"$next"'${2}/s;
  s/(\{\s*path\s*=\s*"(?:crates\/[^"]+|plugins)"\s*,\s*version\s*=\s*")\Q'"$current"'\E(")/${1}'"$next"'${2}/g;
' Cargo.toml
if [ "$(workspace_version)" != "$next" ]; then
  echo "bump: could not rewrite [workspace.package] version in Cargo.toml" >&2
  exit 1
fi
pins=$(grep -cE "path = \"(crates/[^\"]+|plugins)\", version = \"$next\"" Cargo.toml || true)
stale=$(grep -cE "path = \"(crates/[^\"]+|plugins)\", version = \"$current\"" Cargo.toml || true)
if [ "$stale" -ne 0 ]; then
  echo "bump: $stale internal pin(s) in Cargo.toml still say $current" >&2
  exit 1
fi

set_json_version npm/neosh/package.json "$next" pins

for move in "${moves[@]+"${moves[@]}"}"; do
  dir="${move%%=*}"
  to="${move#*=}"
  set_json_version "$dir/package.json" "$to"
  set_lock_version "$dir/package-lock.json" "$to"
done

# So Cargo.lock agrees before anything is committed. Offline first: the workspace's own members are
# all this touches, and a release should not wait on the index for a number it already knows.
cargo update -w --offline >/dev/null 2>&1 || cargo update -w >/dev/null

echo "written: Cargo.toml ($pins pins), Cargo.lock, npm/neosh/package.json, ${#moves[@]} npm package(s)"
if [ "${#firsts[@]}" -gt 0 ]; then
  echo "first publish by hand, before or right after the tag: ${firsts[*]}"
  echo "  (cd plugins/builtin/<name> && npm publish --access public)"
  echo "  npm trust github @neosh/<name> --file publish-npm.yml --repo neoswarm/neosh --allow-publish"
fi

if ! $tag; then
  echo
  echo "next: ./scripts/check.sh, then scripts/bump.sh is done — commit and tag:"
  echo "  git commit -am \"release: v$next\" && git tag -a v$next -m v$next && git push --follow-tags"
  echo "  (or run this again next time with --tag / --push to have it do that)"
  exit 0
fi

git add Cargo.toml Cargo.lock npm/neosh/package.json
for move in "${moves[@]+"${moves[@]}"}"; do
  dir="${move%%=*}"
  git add "$dir/package.json"
  [ -f "$dir/package-lock.json" ] && git add "$dir/package-lock.json"
done
git commit -q -m "release: v$next"
# Annotated: `--follow-tags` silently leaves a lightweight tag behind, and here that means nothing
# publishes at all.
git tag -a "v$next" -m "v$next"
echo "committed \`release: v$next\` and tagged v$next"

if $push; then
  git push --follow-tags
  echo "pushed — tag.yml, release.yml, publish-crates.yml and publish-npm.yml take it from here"
else
  echo "push it to release: git push --follow-tags"
fi

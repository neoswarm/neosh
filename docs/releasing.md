# Releasing neosh

Three registries and one of them is the point: crates.io carries the eleven Rust crates, npm carries
the plugin API and the builtins, and GitHub Releases carries the binaries almost everybody actually
installs.

After the first release, all three are **tokenless**: publishing is a GitHub Actions job whose OIDC
identity crates.io and npm have been told to trust. There is no `CARGO_REGISTRY_TOKEN` and no
`NPM_TOKEN` in the repository secrets, and nothing to rotate.

Getting there takes one manual release, because **neither registry will attach a trusted publisher
to a package that does not exist yet**. That is the whole of the bootstrap below, and it happens
once.

---

## What is published

| Where | What | How many |
|---|---|---|
| crates.io | `neosh`, `neosh-proto`, `neosh-core`, `neosh-provider`, `neosh-vcs`, `neosh-swarm`, `neosh-agent`, `neosh-script`, `neosh-syntax`, `neosh-tui`, `neosh-plugins` | 11 |
| npm | `@neosh/api`, and `@neosh/<name>` for each of the ten bundled plugins | 11 |
| npm | `neosh` — a name reservation that installs nothing (`npm/neosh`) | 1 |
| GitHub Releases | `neosh-<target>.tar.gz` for four macOS and Linux targets | 4 |

Not PyPI: the name is taken by an unrelated Python shell, and a Rust binary has no business there.
Not Windows: the workspace talks to its terminals over a Unix socket, so there is nothing to ship.

Every version is the one number in `[workspace.package]` in the root `Cargo.toml`, and the npm
packages carry it too.

---

## The first release, by hand

Do this once. Everything after it is [the ordinary release](#every-release-after-that).

### 1. Claim the npm scope

`@neosh/*` needs an npm organisation called `neosh` to exist before anything can be published into
it. Create it at <https://www.npmjs.com/org/create> — free for public packages — and turn on 2FA
for the account that owns it.

### 2. Check the whole thing packages

```sh
./scripts/check.sh
cargo publish --workspace --dry-run
```

The dry run is the one that matters here. It builds every crate from the `.crate` file it would
upload rather than from the working directory, which is exactly the difference that used to be
invisible: `neosh-script` embedded `../../plugins`, that built fine here and packaged to nothing,
and the crate would have gone to crates.io with no plugins and no API source in it. `plugins/` is
its own crate now — see `plugins/lib.rs` — so the tree is inside a package root and gets packaged.

### 3. Publish the crates

Get a token from <https://crates.io/settings/tokens>, scoped to `publish-new` and `publish-update`.

```sh
CARGO_REGISTRY_TOKEN=cio... cargo publish --workspace
```

`--workspace` resolves the dependency graph itself, so `neosh-proto` goes up before `neosh-core`
goes up before `neosh`, and it waits for the index between them.

### 4. Publish the npm packages

Get a **granular** access token from <https://www.npmjs.com/settings/~/tokens> with read-and-write
on the `@neosh` scope and on the package `neosh`.

```sh
npm install -g npm@latest        # trusted publishing needs npm >= 11.5.1
NODE_AUTH_TOKEN=npm_... bash -c '
  for dir in npm/neosh plugins/api plugins/builtin/*/; do
    (cd "$dir" && npm publish --access public)
  done'
```

### 5. Turn on trusted publishing, eleven times and then twelve more

This is the tedious part and it only happens once.

**crates.io** — for each of the eleven crates, go to `https://crates.io/crates/<name>/settings`,
add a Trusted Publisher, and fill in:

| Field | Value |
|---|---|
| Repository owner | `neoswarm` |
| Repository name | `neosh` |
| Workflow filename | `publish-crates.yml` |
| Environment | *(leave empty)* |

**npm** — for each of the twelve packages, go to the package's Settings tab, and under Publishing
access choose a trusted publisher:

| Field | Value |
|---|---|
| Provider | GitHub Actions |
| Organization | `neoswarm` |
| Repository | `neosh` |
| Workflow filename | `publish-npm.yml` |
| Environment | *(leave empty)* |

While you are there, set "Require two-factor authentication or automation tokens" — with a trusted
publisher configured, this is what stops a stolen token republishing over it.

### 6. Revoke the two tokens

Both of them, now, at the pages you made them on. They have done their job, and the workflows do
not read them: `publish-crates.yml` exchanges OIDC through `rust-lang/crates-io-auth-action`, and
`publish-npm.yml` lets npm do the exchange itself.

### 7. Tag it and publish the GitHub release

```sh
git tag v0.1.0 && git push --follow-tags
gh release create v0.1.0 --title v0.1.0 --generate-notes
```

Publishing the release is what builds the binaries. It also fires the two publish workflows over
versions you have just put up by hand — both are written to skip whatever the registry already has,
so they run green and do nothing, and the only job that does real work is `release.yml`. That is the
same property that lets you re-run a release which died halfway through.

---

## Every release after that

```sh
# 1. Bump the one version.
$EDITOR Cargo.toml                       # [workspace.package] version
$EDITOR plugins/api/package.json plugins/builtin/*/package.json

# 2. Prove it.
./scripts/check.sh

# 3. Tag it.
git commit -am "release: v0.2.0" && git tag v0.2.0 && git push --follow-tags
```

Then publish the GitHub release for that tag. Publishing it is the trigger: `release.yml` builds
the four binaries and attaches them, `publish-crates.yml` pushes the eleven crates, and
`publish-npm.yml` pushes the npm packages, skipping anything the registry already has — so it is
safe to re-run a half-failed release.

A new crate or a new bundled plugin needs its own first manual publish and its own trusted
publisher, exactly as above. Nothing else does.

---

## Later, and not on the critical path

- **A Homebrew tap.** `neoswarm/homebrew-tap`, with a formula pointing at the release tarballs, so
  `brew install neoswarm/tap/neosh` works. homebrew-core has notability thresholds a new project
  will not meet; the tap has none and is the same command to the user.
- **AUR.** A `neosh-bin` package built from the same tarballs.
- **Nix.** A flake in the repository, then nixpkgs when it settles.

All three consume the GitHub release, which is why that workflow comes first.

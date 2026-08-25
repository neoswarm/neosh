#!/usr/bin/env node
"use strict";

// The binary lives in a platform package — `@neosh/cli-darwin-arm64` and its three siblings — each
// declaring `os` and `cpu`, so npm installs exactly the one that runs here and skips the rest. This
// file is the `bin` npm puts on PATH, and all it does is find that binary and become it.
//
// No `postinstall` download, deliberately: a script that fetches at install time fails closed under
// `--ignore-scripts`, which is the default in an increasing number of CI setups and the first thing
// a security review turns on. An optional dependency is resolved by npm itself, offline caches
// work, and a lockfile pins the binary the way it pins everything else.

const { spawn } = require("node:child_process");

const TARGETS = {
  "darwin arm64": "@neosh/cli-darwin-arm64",
  "darwin x64": "@neosh/cli-darwin-x64",
  "linux x64": "@neosh/cli-linux-x64",
  "linux arm64": "@neosh/cli-linux-arm64",
};

const key = `${process.platform} ${process.arch}`;
const pkg = TARGETS[key];

if (!pkg) {
  // Windows lands here, and says why rather than looking broken: the workspace talks to its
  // terminals over a Unix socket, so there is no binary to have installed.
  const what = process.platform === "win32" ? "Windows" : key;
  console.error(`neosh: no prebuilt binary for ${what}.`);
  console.error("neosh runs on macOS and Linux. See https://github.com/neoswarm/neosh");
  process.exit(1);
}

let binary;
try {
  binary = require.resolve(`${pkg}/bin/neosh`);
} catch {
  // npm skips an optional dependency whose install failed and carries on, so a missing platform
  // package is an ordinary outcome rather than an impossible one — and the fix is a command.
  console.error(`neosh: the ${pkg} package is missing.`);
  console.error("Reinstall with `npm install -g neosh`, or take a binary from");
  console.error("https://github.com/neoswarm/neosh/releases");
  process.exit(1);
}

// `spawn` with inherited stdio rather than `spawnSync`: neosh is a full-screen terminal program
// that wants the tty, and it has signals to receive — `^C` has to reach the agent, not this shim.
const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });

// Forward the signals a terminal sends, so `^C` and a `kill` both land on the real process. The
// shim staying alive until the child is gone is what makes `neosh` in a script behave like neosh.
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT"]) {
  process.on(signal, () => child.kill(signal));
}

child.on("error", (err) => {
  console.error(`neosh: could not start ${binary}: ${err.message}`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  // A process killed by a signal did not exit with a code, and reporting 0 there would tell a
  // shell the run succeeded. 128 + n is what a shell reports for exactly this.
  if (signal) process.exit(128 + (require("node:os").constants.signals[signal] ?? 0));
  process.exit(code ?? 0);
});

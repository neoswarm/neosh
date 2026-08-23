//! `neosh plugin ...`, against a real git repository and a real workspace.
//!
//! The claim being tested is the one the whole program rests on: somebody else's plugin can end up
//! on this machine and run. That is not provable by unit test — it is a clone, a manifest read by
//! the same code startup reads, a directory the discovery path actually searches, and a plugin
//! whose `activate` runs. So this drives the binary, and finishes by starting a workspace and
//! looking for what the installed plugin said.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Sandbox {
    root: PathBuf,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("neosh-pm-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["config", "data", "state", "src", "work"] {
            std::fs::create_dir_all(root.join(d)).expect("sandbox dirs");
        }
        Self { root }
    }

    /// A git repository holding one plugin, which is what a plugin author publishes.
    fn publish(&self, manifest: &str, entry: &str) -> String {
        let src = self.root.join("src");
        std::fs::write(src.join("plugin.toml"), manifest).expect("manifest");
        std::fs::write(src.join("main.ts"), entry).expect("entry");
        git(&src, &["init", "-q", "."]);
        git(&src, &["add", "-A"]);
        git(&src, &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "publish",
        ]);
        format!("file://{}", src.display())
    }

    fn neosh(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_neosh"))
            .arg("--config-dir")
            .arg(self.root.join("config"))
            .args(args)
            .env("NEOSH_DATA_DIR", self.root.join("data"))
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            // A test must never reach the developer's own workspace, and the socket is keyed by
            // the config directory — which is already a sandbox — but the data directory is where
            // an install lands, so it gets one of its own too.
            .output()
            .expect("neosh runs")
    }

    /// Start a workspace and watch its event stream for `needle`.
    ///
    /// Started rather than run-to-completion: plugins activate on their own thread and a process
    /// given no stdin exits before the first one has finished, so `output()` reliably captures a
    /// screen with nothing on it.
    fn workspace_says(&self, needle: &str) -> bool {
        use std::io::{BufRead, BufReader};
        let mut child = Command::new(env!("CARGO_BIN_EXE_neosh"))
            .args(["--ui-protocol", "stdio", "--no-daemon"])
            .arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.root.join("work"))
            .env("NEOSH_DATA_DIR", self.root.join("data"))
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("neosh runs");
        let out = child.stdout.take().expect("stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut found = false;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(line) if line.contains(needle) => {
                    found = true;
                    break;
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        found
    }

    fn plugin(&self, args: &[&str]) -> (bool, String) {
        let mut all = vec!["plugin"];
        all.extend_from_slice(args);
        let out = self.neosh(&all);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").current_dir(dir).args(args).output().expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

const PLUGIN: &str = r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  neosh.notify("the installed plugin ran");
}
"#;

const MANIFEST: &str = "name = \"greeter\"\nversion = \"0.2.0\"\nentry = \"main.ts\"\n\
                        permissions = [\"tools\"]\n";

/// The whole point, end to end: published, installed, listed, loaded, removed.
#[test]
fn a_plugin_published_as_a_git_repository_installs_and_runs() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("roundtrip");
    let url = sb.publish(MANIFEST, PLUGIN);

    let (ok, said) = sb.plugin(&["add", &url]);
    assert!(ok, "install failed: {said}");
    assert!(said.contains("greeter"), "it says what it installed: {said}");
    assert!(said.contains("0.2.0"), "and which version: {said}");
    // What it may do, read from the manifest — which is now enforced rather than described, so it
    // is a promise the user can hold neosh to and worth printing at the moment of consent.
    assert!(said.contains("tools"), "and what it declared it may do: {said}");

    let (ok, listed) = sb.plugin(&["list"]);
    assert!(ok, "list failed: {listed}");
    assert!(listed.contains("greeter"), "the installed plugin is listed: {listed}");
    assert!(listed.contains("installed"), "and says where it came from: {listed}");
    assert!(listed.contains(&url), "and the remote it came from: {listed}");
    // Bundled ones are listed too. "What loads here" is one question with one answer, and a list
    // that showed only the installed ones would make the bundled ones look like they were not
    // plugins — which is the opposite of what they are.
    assert!(listed.contains("sidebar"), "bundled plugins are plugins: {listed}");

    // And it actually loads: the discovery path really searches the directory `add` wrote to.
    // This is the assertion the rest of the command exists to make true, and the only one that
    // cannot be faked by the installer agreeing with itself.
    assert!(
        sb.workspace_says("the installed plugin ran"),
        "an installed plugin has to actually load"
    );

    let (ok, gone) = sb.plugin(&["remove", "greeter", "--yes"]);
    assert!(ok, "remove failed: {gone}");
    let (_, listed) = sb.plugin(&["list"]);
    assert!(!listed.contains("greeter"), "it is still listed after removal: {listed}");
}

/// Installing something is running its code later, so what fails must fail *here*.
///
/// A plugin built against another protocol, or missing the file its manifest names, used to be
/// discovered at the next startup — where it is a line in a log, during something else, about a
/// plugin the person may have installed days earlier.
#[test]
fn a_plugin_that_would_not_load_is_refused_at_install_time() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("badentry");
    // The manifest names a file the repository does not contain.
    let url = sb.publish("name = \"ghost\"\nversion = \"0.1.0\"\nentry = \"nope.ts\"\n", PLUGIN);

    let (ok, said) = sb.plugin(&["add", &url]);
    assert!(!ok, "a broken plugin installed anyway: {said}");
    assert!(said.contains("not a neosh plugin"), "and says why: {said}");

    // And nothing was left behind — neither the plugin nor the scratch directory the clone went
    // into. A `.adding-4181` in the plugin path is a thing nobody can interpret, and it would make
    // the next attempt at the same plugin fail for a different reason.
    let (_, listed) = sb.plugin(&["list"]);
    assert!(!listed.contains("ghost"), "a refused plugin is listed: {listed}");
    let leftovers: Vec<String> = std::fs::read_dir(sb.root.join("data/plugins"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(leftovers.is_empty(), "the failed clone was left behind: {leftovers:?}");
}

/// Deleting is irreversible, so it asks — and with nobody there to ask, it refuses.
///
/// The alternative is a command that quietly deletes when run from a script and prompts when run
/// by hand, which is a command whose behaviour you cannot predict from what you typed.
#[test]
fn removing_a_plugin_will_not_happen_unasked() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("confirm");
    let url = sb.publish(MANIFEST, PLUGIN);
    assert!(sb.plugin(&["add", &url]).0);

    let (ok, said) = sb.plugin(&["remove", "greeter"]);
    assert!(!ok, "it deleted without being asked: {said}");
    assert!(said.contains("cannot be undone"), "and says what was at stake: {said}");
    assert!(said.contains("--yes"), "and how to mean it: {said}");

    let (_, listed) = sb.plugin(&["list"]);
    assert!(listed.contains("greeter"), "it was removed anyway: {listed}");
}

/// A plugin the user put in their own config directory is theirs.
///
/// `add` did not put it there and `remove` does not get to take it away — the directory is
/// somebody's working copy, and an editor that deletes one because a name matched is an editor you
/// stop trusting with a filesystem.
#[test]
fn a_hand_managed_plugin_is_not_neoshs_to_delete() {
    let sb = Sandbox::new("handmade");
    let dir = sb.root.join("config/plugins/mine");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("plugin.toml"), "name = \"mine\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n")
        .expect("manifest");
    std::fs::write(dir.join("main.ts"), PLUGIN).expect("entry");

    let (_, listed) = sb.plugin(&["list"]);
    assert!(listed.contains("mine"), "a hand-managed plugin is still a plugin: {listed}");
    assert!(listed.contains("yours"), "and is marked as yours: {listed}");

    let (ok, said) = sb.plugin(&["remove", "mine", "--yes"]);
    assert!(!ok, "it deleted a directory it did not install: {said}");
    assert!(dir.join("plugin.toml").is_file(), "the plugin was deleted");
}

/// A local directory is not installed by copying it.
///
/// Two copies of a plugin you are writing is two things to keep in step and no way to tell which
/// one is being edited. The directory neosh already discovers is the better answer, so the refusal
/// is the one that names it.
#[test]
fn installing_from_a_path_says_where_a_plugin_you_are_writing_belongs() {
    let sb = Sandbox::new("frompath");
    let (ok, said) = sb.plugin(&["add", "./somewhere"]);
    assert!(!ok, "a path was cloned: {said}");
    assert!(said.contains("config/plugins"), "it names the directory to use instead: {said}");
}

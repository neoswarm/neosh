//! Where an API key comes from, and how one you type gets kept.
//!
//! # The rule this file exists to enforce
//!
//! **A secret is never written to a file neosh controls, and never leaves this crate as a
//! `String`.** Everything that leaves is a [`CredentialSource`] — a statement about *where* a key
//! is, never what it is. That is why [`CredentialInfo`] has no value field and why nothing here
//! returns one to the API layer.
//!
//! # The four places a key can be
//!
//! | Source | Survives a restart | Set by |
//! |---|---|---|
//! | [`CredentialSource::Env`] | as long as your shell exports it | you, outside neosh |
//! | [`CredentialSource::Command`] | yes | `auth = { kind = "command", argv = [...] }` |
//! | [`CredentialSource::Keychain`] | yes | typing one in, when a keychain helper exists |
//! | [`CredentialSource::Session`] | no | typing one in, when it does not |
//!
//! The order is deliberate: a key you typed *in this session* outranks the environment, because you
//! typed it after seeing what the environment gave you. Everything else is consulted in the order a
//! config file would suggest.
//!
//! # Why the keychain is a subprocess
//!
//! `secret-tool` and `security` are already installed, already unlocked with the login session, and
//! already audited. Linking a keychain library would add a large dependency tree to do the same
//! thing less portably, and would still shell out on the platforms where the library is a stub.
//! Storing goes over the helper's **stdin**, never its argv, so the key is not visible in `ps`.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};

use neosh_proto::{AuthRef, CredentialInfo, CredentialSource, InstanceConfig, InstanceId};


use secrecy::{ExposeSecret, SecretString};

/// The service name every keychain entry is filed under.
const SERVICE: &str = "neosh";

/// The process-wide store.
///
/// Global because a driver is handed an [`InstanceConfig`] and a request, not a host: threading a
/// store through every `stream()` signature would put a credential parameter on the one trait third
/// parties implement, and the first thing a third party would do is log it.
static STORE: LazyLock<Arc<Credentials>> = LazyLock::new(|| Arc::new(Credentials::detect()));

/// The store every driver resolves against.
pub fn credentials() -> Arc<Credentials> {
    STORE.clone()
}

/// Which keychain helper this machine has, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keychain {
    /// `secret-tool`, from libsecret. Freedesktop, so most Linux desktops.
    SecretTool,
    /// `security`, on macOS.
    MacSecurity,
}

impl Keychain {
    fn detect() -> Option<Self> {
        // Order by platform likelihood rather than preference: a machine has one of these, not a
        // choice between them.
        if cfg!(target_os = "macos") && which("security").is_some() {
            return Some(Self::MacSecurity);
        }
        which("secret-tool").map(|_| Self::SecretTool)
    }

    fn load(self, account: &str) -> Option<SecretString> {
        let out = match self {
            Self::SecretTool => Command::new("secret-tool")
                .args(["lookup", "service", SERVICE, "account", account])
                .output(),
            Self::MacSecurity => Command::new("security")
                .args(["find-generic-password", "-s", SERVICE, "-a", account, "-w"])
                .output(),
        }
        .ok()?;
        if !out.status.success() {
            return None;
        }
        // Trailing newline is the helper's, not the key's. A key with a stray `\n` produces a 401
        // that looks like a wrong key, which is an afternoon of debugging the wrong thing.
        let value = String::from_utf8(out.stdout).ok()?;
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| SecretString::from(trimmed))
    }

    /// Write to the keychain, passing the secret on **stdin** so it never appears in `ps`.
    fn store(self, account: &str, secret: &SecretString) -> Result<(), String> {
        let mut cmd = match self {
            Self::SecretTool => {
                let mut c = Command::new("secret-tool");
                c.args([
                    "store",
                    "--label",
                    &format!("neosh: {account}"),
                    "service",
                    SERVICE,
                    "account",
                    account,
                ]);
                c
            }
            // `-i` reads commands from stdin. It is the only way to hand `security` a password
            // without putting it in argv, where every other process on the machine can read it.
            Self::MacSecurity => {
                let mut c = Command::new("security");
                c.arg("-i");
                c
            }
        };
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;

        let payload = match self {
            Self::SecretTool => secret.expose_secret().to_string(),
            Self::MacSecurity => format!(
                "add-generic-password -U -s {SERVICE} -a {account} -w {}\n",
                secret.expose_secret()
            ),
        };
        {
            let mut stdin = child.stdin.take().ok_or_else(|| "no stdin".to_string())?;
            stdin.write_all(payload.as_bytes()).map_err(|e| e.to_string())?;
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        // Nothing from the helper is logged: its diagnostics can quote what it was given.
        if status.success() { Ok(()) } else { Err("the keychain helper refused it".into()) }
    }

    fn forget(self, account: &str) {
        let _ = match self {
            Self::SecretTool => Command::new("secret-tool")
                .args(["clear", "service", SERVICE, "account", account])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
            Self::MacSecurity => Command::new("security")
                .args(["delete-generic-password", "-s", SERVICE, "-a", account])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        };
    }
}

/// Where a stored key ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// In the OS keychain. It will be there next time.
    Keychain,
    /// In memory for this run only, because there is no keychain helper on this machine.
    Session,
}

#[derive(Debug)]
pub struct Credentials {
    keychain: Option<Keychain>,
    /// Keys typed in this session, and every keychain answer already paid for.
    ///
    /// `None` means "looked, and there was nothing" — caching the *misses* is the part that
    /// matters. A settings list asks about every configured instance, and most of them have no key;
    /// without remembering that, drawing the model picker spawns `secret-tool` a dozen times, on
    /// the loop that is also drawing the screen.
    memory: Mutex<HashMap<InstanceId, Option<Entry>>>,
}

#[derive(Debug, Clone)]
struct Entry {
    secret: SecretString,
    from: Stored,
}

impl Default for Credentials {
    fn default() -> Self {
        Self { keychain: None, memory: Mutex::default() }
    }
}

impl Credentials {
    /// A store that uses whatever keychain helper is installed.
    pub fn detect() -> Self {
        Self { keychain: Keychain::detect(), memory: Mutex::default() }
    }

    /// A store with no keychain at all, for tests and for `--no-keychain`.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Whether a key typed in will still be here tomorrow.
    pub fn is_durable(&self) -> bool {
        self.keychain.is_some()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<InstanceId, Option<Entry>>> {
        // A poisoned lock means a panic happened while a key was in scope. Taking the map anyway
        // is right: the alternative is that one panic makes the editor unable to authenticate for
        // the rest of the run.
        self.memory.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Look up a key that was typed in, or is in the keychain. Not the environment — that is
    /// [`AuthRef`]'s business, and it is consulted by [`resolve`](Self::resolve).
    ///
    /// `ask_keychain` is false for instances that have nowhere to put a key: a CLI login and a
    /// local endpoint would never have one, and asking costs a process launch to be told so.
    fn stored(&self, id: &InstanceId, ask_keychain: bool) -> Option<Entry> {
        if let Some(known) = self.lock().get(id) {
            return known.clone();
        }
        let found = if ask_keychain {
            self.keychain.and_then(|k| k.load(id.as_ref()))
        } else {
            None
        };
        let entry = found.map(|secret| Entry { secret, from: Stored::Keychain });
        // Remembered either way — including the miss, which is the common answer.
        self.lock().insert(id.clone(), entry.clone());
        entry
    }

    /// Keep a key.
    ///
    /// Never fails: a keychain that refuses — no session bus, a locked keyring — falls back to
    /// memory rather than throwing the key away. What comes back says which happened, so the
    /// caller can be honest about whether it will still be there tomorrow.
    pub fn set(&self, id: &InstanceId, secret: SecretString) -> Stored {
        let from = match self.keychain {
            Some(k) => match k.store(id.as_ref(), &secret) {
                Ok(()) => Stored::Keychain,
                Err(e) => {
                    // The helper's message, not the value: nothing here has ever seen one.
                    tracing::warn!("the keychain helper refused to store a key: {e}");
                    Stored::Session
                }
            },
            None => Stored::Session,
        };
        self.lock().insert(id.clone(), Some(Entry { secret, from }));
        from
    }

    /// Drop a key from memory and from the keychain. The environment is not ours to clear.
    pub fn forget(&self, id: &InstanceId) {
        // Recorded as a miss rather than removed, so the next look does not go back to the
        // keychain for something we just took out of it.
        self.lock().insert(id.clone(), None);
        if let Some(k) = self.keychain {
            k.forget(id.as_ref());
        }
    }

    /// The key to send for this instance, and where it came from.
    ///
    /// `Ok(None)` means the instance legitimately needs no key. `Err` means it needs one and there
    /// is none, which is a different thing and gets a different message.
    pub fn resolve(
        &self,
        id: &InstanceId,
        auth: &AuthRef,
    ) -> Result<Option<SecretString>, crate::ProviderError> {
        if let Some(entry) = self.stored(id, accepts_key(auth)) {
            return Ok(Some(entry.secret));
        }
        match auth {
            // A plan carries its own login. There is nothing here to resolve, and nothing neosh
            // could hand the driver if there were.
            AuthRef::None | AuthRef::Inherited | AuthRef::Cli { .. } => Ok(None),
            AuthRef::Env { var } => match std::env::var(var) {
                Ok(v) if !v.trim().is_empty() => Ok(Some(SecretString::from(v.trim()))),
                _ => Err(crate::ProviderError::MissingCredentials(format!(
                    "{id} has no key: ${var} is not set, and none has been entered"
                ))),
            },
            AuthRef::Command { argv } => match run_helper(argv) {
                Some(s) => Ok(Some(s)),
                None => Err(crate::ProviderError::MissingCredentials(format!(
                    "{id}: the credential helper {:?} produced nothing",
                    argv.first().map(String::as_str).unwrap_or("")
                ))),
            },
        }
    }

    /// Where this instance's key is coming from, for a settings list. Reads nothing secret into
    /// anything that leaves this function.
    pub fn source(&self, cfg: &InstanceConfig) -> CredentialSource {
        if let Some(entry) = self.stored(&cfg.id, accepts_key(&cfg.auth)) {
            // Deliberately drops the value on the floor; only its provenance escapes.
            return match entry.from {
                Stored::Keychain => CredentialSource::Keychain,
                Stored::Session => CredentialSource::Session,
            };
        }
        match &cfg.auth {
            AuthRef::None => CredentialSource::NotNeeded,
            AuthRef::Inherited => CredentialSource::Inherited,
            // Whether the CLI is *installed* is cheap to answer and worth answering: "run `codex
            // login`" is a different problem from "install codex", and a picker that conflated
            // them would send you to the wrong page. Whether you are signed *in* is deliberately
            // not claimed — finding out means running it, and a wrong guess either way is worse
            // than saying where to look.
            // A plan the vendor has stopped serving is answered before anything is looked up on
            // this machine, because nothing on this machine is what went wrong. Whether the program
            // is installed is not the question and "install it" is not the fix.
            AuthRef::Cli { program, retired: Some(note), .. } => CredentialSource::PlanRetired {
                program: program.clone(),
                note: note.clone(),
            },
            AuthRef::Cli { program, login, .. } => match which(program) {
                Some(_) => CredentialSource::Plan { via: program.clone() },
                None => CredentialSource::PlanMissing {
                    program: program.clone(),
                    hint: login.clone(),
                },
            },
            AuthRef::Command { argv } if !argv.is_empty() => CredentialSource::Command,
            AuthRef::Command { .. } => CredentialSource::Missing,
            AuthRef::Env { var } => {
                if std::env::var_os(var).is_some_and(|v| !v.is_empty()) {
                    CredentialSource::Env { var: var.clone() }
                } else {
                    CredentialSource::Missing
                }
            }
        }
    }

    /// One row per instance, in the order given.
    pub fn survey<'a>(
        &self,
        instances: impl Iterator<Item = &'a InstanceConfig>,
        driver_available: impl Fn(&InstanceConfig) -> bool,
    ) -> Vec<CredentialInfo> {
        instances
            .map(|cfg| CredentialInfo {
                instance: cfg.id.clone(),
                display_name: cfg.display_name.clone(),
                driver: cfg.driver.clone(),
                source: self.source(cfg),
                account: cfg.auth.account_kind(),
                brand: cfg.brand.clone(),
                driver_available: driver_available(cfg),
                accepts_key: accepts_key(&cfg.auth),
            })
            .collect()
    }
}

/// Whether typing a key in for this instance would do anything.
///
/// A CLI login and a local endpoint have nowhere to put one, so offering to take one would be
/// offering to do nothing — and looking one up costs a process launch to be told so.
fn accepts_key(auth: &AuthRef) -> bool {
    !matches!(auth, AuthRef::Inherited | AuthRef::None | AuthRef::Cli { .. })
}

/// Run a credential helper and take its first line.
///
/// First line rather than the whole output, because `pass` prints the password first and metadata
/// after it, and sending the metadata as a bearer token fails in a way that names the wrong thing.
fn run_helper(argv: &[String]) -> Option<SecretString> {
    let (program, args) = argv.split_first()?;
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| SecretString::from(line))
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join(program)).find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::DriverKind;

    fn cfg(id: &str, auth: AuthRef) -> InstanceConfig {
        InstanceConfig {
            id: InstanceId::from(id),
            driver: DriverKind::from("openai-compat"),
            display_name: id.into(),
            base_url: None,
            brand: None,
            auth,
            models: Vec::new(),
            extra_headers: Vec::new(),
        }
    }

    #[test]
    fn a_key_typed_in_is_reported_as_session_only_without_a_keychain() {
        let store = Credentials::in_memory();
        let c = cfg("acme", AuthRef::Env { var: "NEOSH_TEST_UNSET_XYZ".into() });
        assert_eq!(store.source(&c), CredentialSource::Missing);

        assert_eq!(store.set(&c.id, SecretString::from("sk-test")), Stored::Session);
        assert_eq!(store.source(&c), CredentialSource::Session);
        assert!(!store.is_durable(), "no helper on this machine means no promise of persistence");
    }

    #[test]
    fn a_typed_key_outranks_the_environment() {
        // The user typed one *after* seeing what the environment offered, so theirs is the newer
        // statement of intent.
        let store = Credentials::in_memory();
        let c = cfg("acme", AuthRef::Env { var: "PATH".into() }); // PATH is always set and non-empty
        assert!(matches!(store.source(&c), CredentialSource::Env { .. }));
        store.set(&c.id, SecretString::from("typed"));
        assert_eq!(store.source(&c), CredentialSource::Session);
        let got = store.resolve(&c.id, &c.auth).expect("resolves").expect("has a key");
        assert_eq!(got.expose_secret(), "typed");
    }

    #[test]
    fn forgetting_falls_back_to_whatever_the_config_says() {
        let store = Credentials::in_memory();
        let c = cfg("acme", AuthRef::None);
        store.set(&c.id, SecretString::from("typed"));
        assert_eq!(store.source(&c), CredentialSource::Session);
        store.forget(&c.id);
        assert_eq!(store.source(&c), CredentialSource::NotNeeded);
    }

    #[test]
    fn an_instance_with_nowhere_to_put_a_key_is_never_looked_up() {
        // Regression, and a sharp one: a settings list asks about every instance, most of which
        // have no key. Going to the keychain for each of them — and again on the next redraw,
        // because a miss was not remembered — put a dozen process launches on the loop that draws
        // the screen, and startup went from a tenth of a second to timing out.
        let store = Credentials::in_memory();
        for auth in [AuthRef::Inherited, AuthRef::None] {
            let c = cfg("local", auth);
            store.source(&c);
        }
        assert!(
            store.lock().values().all(|v| v.is_none()),
            "nothing was looked up, so nothing is cached as found"
        );
    }

    #[test]
    fn an_instance_that_needs_no_key_resolves_to_none_rather_than_an_error() {
        let store = Credentials::in_memory();
        for auth in [AuthRef::None, AuthRef::Inherited] {
            let c = cfg("local", auth);
            assert!(store.resolve(&c.id, &c.auth).expect("no error").is_none());
        }
    }

    #[test]
    fn a_missing_key_names_the_variable_it_looked_in() {
        let store = Credentials::in_memory();
        let c = cfg("acme", AuthRef::Env { var: "NEOSH_TEST_UNSET_XYZ".into() });
        let err = store.resolve(&c.id, &c.auth).expect_err("must not silently send nothing");
        assert!(err.to_string().contains("NEOSH_TEST_UNSET_XYZ"), "{err}");
    }

    #[test]
    fn a_helper_supplies_the_first_line_and_drops_its_metadata() {
        let store = Credentials::in_memory();
        let c = cfg(
            "acme",
            AuthRef::Command {
                argv: vec!["printf".into(), "sk-line-one\\nusername: someone\\n".into()],
            },
        );
        assert_eq!(store.source(&c), CredentialSource::Command);
        let got = store.resolve(&c.id, &c.auth).expect("resolves").expect("has a key");
        assert_eq!(got.expose_secret(), "sk-line-one");
    }

    #[test]
    fn a_helper_that_fails_is_a_missing_credential_not_an_empty_one() {
        let store = Credentials::in_memory();
        let c = cfg("acme", AuthRef::Command { argv: vec!["false".into()] });
        assert!(store.resolve(&c.id, &c.auth).is_err());
    }

    #[test]
    fn a_survey_row_carries_provenance_and_never_a_value() {
        let store = Credentials::in_memory();
        let c = cfg("acme", AuthRef::Env { var: "NEOSH_TEST_UNSET_XYZ".into() });
        store.set(&c.id, SecretString::from("sk-secret-value"));
        let rows = store.survey([&c].into_iter(), |_| true);
        let json = serde_json::to_string(&rows).expect("serialises");
        assert!(!json.contains("sk-secret-value"), "a secret reached the wire: {json}");
        assert!(rows[0].accepts_key);
    }
}

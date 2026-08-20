//! The permission layer.
//!
//! Every filesystem, network and exec action passes through [`PermissionLayer::check`] before it
//! happens. Non-negotiable for an agent that edits a codebase: without it, a prompt injection in a
//! file the agent reads becomes arbitrary code execution.
//!
//! The interesting part is path containment. Rejecting a literal `..` is not enough — a symlink
//! inside the workspace pointing out of it defeats a purely lexical check, and `canonicalize`
//! cannot be used on its own because it fails for files that do not exist yet, which is exactly the
//! case when the agent is creating one. So we normalize lexically first, then canonicalize the
//! nearest existing ancestor and re-check containment.

use std::path::{Component, Path, PathBuf};

use neosh_proto::{Capability, PermissionDecision, PermissionMode};

#[derive(Debug, Clone)]
pub struct PermissionLayer {
    mode: PermissionMode,
    /// Everything the agent may touch without asking. Paths are workspace-relative.
    workspace: PathBuf,
    allowed_commands: Vec<String>,
    allowed_hosts: Vec<String>,
    /// Extra roots outside the workspace the user has opted into.
    extra_roots: Vec<PathBuf>,
}

impl PermissionLayer {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            mode: PermissionMode::Ask,
            workspace: workspace.into(),
            allowed_commands: Vec::new(),
            allowed_hosts: Vec::new(),
            extra_roots: Vec::new(),
        }
    }

    pub fn with_mode(mut self, mode: PermissionMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn allow_command(mut self, c: impl Into<String>) -> Self {
        self.allowed_commands.push(c.into());
        self
    }

    pub fn allow_host(mut self, h: impl Into<String>) -> Self {
        self.allowed_hosts.push(h.into());
        self
    }

    pub fn allow_root(mut self, p: impl Into<PathBuf>) -> Self {
        self.extra_roots.push(p.into());
        self
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Resolve a possibly-relative path and confirm it stays inside an allowed root.
    ///
    /// Returns the resolved path on success. `None` means the path escapes every root, by any
    /// route including symlinks.
    pub fn resolve_path(&self, raw: &str) -> Option<PathBuf> {
        let joined = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.workspace.join(raw)
        };
        let lexical = normalize(&joined);

        let roots: Vec<PathBuf> = std::iter::once(self.workspace.clone())
            .chain(self.extra_roots.iter().cloned())
            .map(|r| normalize(&r))
            .collect();

        if !roots.iter().any(|r| lexical.starts_with(r)) {
            return None;
        }

        // A symlink can point out of the workspace while looking contained. Check the deepest
        // ancestor that actually exists, which is the most that can be resolved for a file the
        // agent is about to create.
        let mut probe: &Path = &lexical;
        loop {
            if probe.exists() {
                let real = probe.canonicalize().ok()?;
                let real_roots: Vec<PathBuf> =
                    roots.iter().filter_map(|r| r.canonicalize().ok()).collect();
                // If no root exists on disk, the lexical check above is all we have.
                if !real_roots.is_empty() && !real_roots.iter().any(|r| real.starts_with(r)) {
                    return None;
                }
                break;
            }
            match probe.parent() {
                Some(p) if p != probe => probe = p,
                _ => break,
            }
        }
        Some(lexical)
    }

    pub fn check(&self, cap: &Capability) -> PermissionDecision {
        let deny = |reason: String| PermissionDecision::Deny { reason };

        match cap {
            Capability::ReadFile { path } | Capability::WriteFile { path } => {
                let Some(_resolved) = self.resolve_path(path) else {
                    // Containment is enforced in every mode, including `AllowListed`. An escape is
                    // never merely "unapproved" — it is refused outright.
                    return deny(format!(
                        "{path} is outside the workspace ({})",
                        self.workspace.display()
                    ));
                };
                let writing = matches!(cap, Capability::WriteFile { .. });
                match self.mode {
                    PermissionMode::Deny => deny("permission mode is deny".into()),
                    // Still inside the workspace: the check above already refused anything outside
                    // it, in every mode. "Full access" means not being asked, not reaching further.
                    PermissionMode::Allow => PermissionDecision::Allow,
                    // Reads inside the workspace are the agent's basic job; prompting for each one
                    // trains users to approve reflexively, which is worse than not asking.
                    PermissionMode::AllowListed if !writing => PermissionDecision::Allow,
                    PermissionMode::AllowListed => PermissionDecision::Prompt,
                    PermissionMode::Ask if !writing => PermissionDecision::Allow,
                    PermissionMode::Ask => PermissionDecision::Prompt,
                }
            }
            Capability::Exec { command } => {
                let program = command.split_whitespace().next().unwrap_or_default();
                match self.mode {
                    PermissionMode::Deny => deny("permission mode is deny".into()),
                    PermissionMode::Allow => PermissionDecision::Allow,
                    _ if self.allowed_commands.iter().any(|c| c == program) => {
                        PermissionDecision::Allow
                    }
                    PermissionMode::AllowListed => {
                        deny(format!("{program} is not in the allow-list"))
                    }
                    PermissionMode::Ask => PermissionDecision::Prompt,
                }
            }
            Capability::Network { host } => match self.mode {
                PermissionMode::Deny => deny("permission mode is deny".into()),
                PermissionMode::Allow => PermissionDecision::Allow,
                _ if self.allowed_hosts.iter().any(|h| h == host) => PermissionDecision::Allow,
                PermissionMode::AllowListed => deny(format!("{host} is not in the allow-list")),
                PermissionMode::Ask => PermissionDecision::Prompt,
            },
        }
    }
}

/// Lexically normalize a path: resolve `.` and `..` without touching the filesystem.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer() -> PermissionLayer {
        PermissionLayer::new("/work/project")
    }

    #[test]
    fn relative_paths_resolve_inside_the_workspace() {
        assert_eq!(
            layer().resolve_path("src/main.rs"),
            Some(PathBuf::from("/work/project/src/main.rs"))
        );
    }

    #[test]
    fn dot_dot_traversal_is_refused() {
        for evil in ["../secrets", "src/../../etc/passwd", "a/b/../../../outside"] {
            assert_eq!(layer().resolve_path(evil), None, "{evil} should be refused");
        }
    }

    #[test]
    fn absolute_paths_outside_the_workspace_are_refused() {
        assert_eq!(layer().resolve_path("/etc/passwd"), None);
        assert!(layer().resolve_path("/work/project/ok.txt").is_some());
    }

    #[test]
    fn an_extra_root_widens_access_without_widening_it_everywhere() {
        let l = layer().allow_root("/opt/shared");
        assert!(l.resolve_path("/opt/shared/lib.rs").is_some());
        assert_eq!(l.resolve_path("/opt/other/lib.rs"), None);
    }

    #[test]
    fn escaping_the_workspace_is_denied_even_in_allow_listed_mode() {
        let l = layer().with_mode(PermissionMode::AllowListed);
        let d = l.check(&Capability::ReadFile { path: "../../etc/passwd".into() });
        assert!(matches!(d, PermissionDecision::Deny { .. }), "got {d:?}");
    }

    #[test]
    fn deny_mode_refuses_everything() {
        let l = layer().with_mode(PermissionMode::Deny).allow_command("ls");
        for cap in [
            Capability::ReadFile { path: "a.txt".into() },
            Capability::Exec { command: "ls".into() },
            Capability::Network { host: "example.com".into() },
        ] {
            assert!(matches!(l.check(&cap), PermissionDecision::Deny { .. }), "{cap:?}");
        }
    }

    #[test]
    fn reads_in_the_workspace_are_allowed_but_writes_prompt() {
        let l = layer();
        assert_eq!(l.check(&Capability::ReadFile { path: "a.txt".into() }), PermissionDecision::Allow);
        assert_eq!(
            l.check(&Capability::WriteFile { path: "a.txt".into() }),
            PermissionDecision::Prompt
        );
    }

    #[test]
    fn allow_listed_commands_skip_the_prompt_others_do_not() {
        let l = layer().with_mode(PermissionMode::AllowListed).allow_command("cargo");
        assert_eq!(
            l.check(&Capability::Exec { command: "cargo test".into() }),
            PermissionDecision::Allow
        );
        assert!(matches!(
            l.check(&Capability::Exec { command: "rm -rf /".into() }),
            PermissionDecision::Deny { .. }
        ));
    }

    #[test]
    fn a_symlink_pointing_out_of_the_workspace_is_refused() {
        let tmp = std::env::temp_dir().join(format!("neosh-perm-{}", std::process::id()));
        let ws = tmp.join("ws");
        let outside = tmp.join("outside");
        let _ = std::fs::create_dir_all(&ws);
        let _ = std::fs::create_dir_all(&outside);
        let _ = std::fs::write(outside.join("secret.txt"), "s3cret");
        let link = ws.join("escape");
        let _ = std::fs::remove_file(&link);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link).expect("symlink");
            let l = PermissionLayer::new(&ws);
            // Looks contained lexically, but resolves outside.
            assert_eq!(l.resolve_path("escape/secret.txt"), None);
            // A genuinely contained path still works.
            let _ = std::fs::write(ws.join("real.txt"), "ok");
            assert!(l.resolve_path("real.txt").is_some());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

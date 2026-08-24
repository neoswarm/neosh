//! The calls that take long enough to matter: git, and one-shot generation.
//!
//! Both are handled off the host loop. That is not an optimisation — the host loop is the single
//! writer for the editor, so awaiting a `git status` inside it freezes typing, and awaiting a model
//! round-trip freezes it for seconds. Each of these becomes a spawned task that reports back
//! through the same request id the plugin is already waiting on.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use std::collections::HashMap;
use std::sync::{Arc as StdArc, Mutex};

use neosh_agent::Agent;
use neosh_proto::{ApiError, ApiOk, ApiResult, DiffTarget, InstanceId, ModelEntry, ModelSelection};
use neosh_vcs::{Git, VcsError};
use tokio_util::sync::CancellationToken;

/// Everything a spawned call needs, cheap to clone into a task.
#[derive(Clone)]
pub struct Services {
    pub agent: Arc<Agent>,
    /// `None` when neosh was started outside a repository. Every git call then answers with a
    /// `NotFound` naming the directory, rather than the host pretending git is broken.
    pub git: Option<Git>,
    pub cwd: PathBuf,
    /// `gen.model`, as `instance/model`. Empty means "use whatever the session is using".
    pub gen_model: String,
    /// Whether the calling plugin declared `vcs_write` in its manifest.
    pub may_write_vcs: bool,
    /// Who is calling, for the refusal message.
    pub caller: String,
    /// Where conversations are written, when they are written anywhere at all.
    ///
    /// `None` in a workspace with no state directory — the test harnesses, mostly — and then
    /// there is nothing on disk to describe, which is an empty answer rather than an error.
    pub state_dir: Option<PathBuf>,
    /// The plugin boundary, so a permission question can reach whoever answers it.
    pub bridge: StdArc<crate::bridge::ScriptBridge>,
    /// Models discovered from each endpoint this session.
    ///
    /// Shared with the host so it survives across calls and is cleared on reload. Without it every
    /// press of the model-picker key is one network round trip per configured provider, which is
    /// seconds of a picker that has not appeared yet.
    pub model_cache: ModelCache,
}

/// Discovered models per instance, for the life of a session.
pub type ModelCache = StdArc<Mutex<HashMap<InstanceId, Vec<neosh_proto::ModelInfo>>>>;

impl Services {
    fn repo(&self) -> Result<&Git, ApiError> {
        self.git.as_ref().ok_or_else(|| ApiError::NotFound {
            what: format!("no git repository at {}", self.cwd.display()),
        })
    }

    /// The repository at `cwd`, or this conversation's when nobody asked for one in particular.
    ///
    /// Discovery is one `rev-parse`, which is why a caller may name a directory per call rather
    /// than the host keeping a table of open repositories: the panel asks about a project the
    /// cursor is on, and the answer is stale the moment the cursor moves.
    async fn repo_at(&self, cwd: Option<String>) -> Result<Cow<'_, Git>, ApiError> {
        let Some(path) = cwd else {
            return Ok(Cow::Borrowed(self.repo()?));
        };
        Git::discover(&path)
            .await
            .map(Cow::Owned)
            .ok_or_else(|| ApiError::NotFound { what: format!("no git repository at {path}") })
    }

    /// Refuse a repository write from a plugin that did not declare it.
    ///
    /// Checked against the **manifest**, not the agent's permission mode. Those modes govern what
    /// the model may do; the model does not arrive here — it calls a registered tool, which is
    /// gated like every tool. A user pressing Enter in a branch picker is not the agent acting, and
    /// asking the agent's policy about it would refuse the thing the user just asked for.
    fn permit_write(&self, verb: &str) -> Result<(), ApiError> {
        if self.may_write_vcs {
            return Ok(());
        }
        Err(ApiError::NotPermitted {
            capability: format!(
                "vcs_write (plugin {} tried to run `git {verb}`; add \"vcs_write\" to its \
                 plugin.toml permissions)",
                self.caller
            ),
        })
    }

    // ---- conversations on disk ------------------------------------------

    /// Every conversation in the state directory, loaded or not.
    ///
    /// Off the host loop because it parses every file, and off the *async* worker as well: this is
    /// blocking file work, and a runtime thread sitting in `read_to_string` four hundred times is
    /// one that is not driving anything else.
    pub async fn sessions_stored(&self) -> ApiResult {
        let Some(dir) = self.state_dir.clone() else {
            return Ok(ApiOk::Sessions { sessions: Vec::new() });
        };
        let sessions = tokio::task::spawn_blocking(move || crate::sessions::scan(&dir))
            .await
            .map_err(|e| ApiError::Internal { message: e.to_string() })?;
        Ok(ApiOk::Sessions { sessions })
    }

    // ---- git ------------------------------------------------------------

    /// Directories whose path begins with `prefix`.
    ///
    /// One `read_dir` on the parent, filtered by the last segment. Not recursive and not a glob:
    /// completing a path should cost the same whether you are in `/tmp` or `/`, and a plugin asking
    /// for `/` should not walk the disk.
    ///
    /// Directory names only. Nothing here reads a file, and the cap means a directory with fifty
    /// thousand entries answers in bounded time rather than freezing whatever asked.
    pub async fn path_complete(&self, prefix: String) -> ApiResult {
        const LIMIT: usize = 200;

        let expanded = expand_home(&prefix);
        // A trailing separator means "inside this directory", not "starting with this name".
        let (dir, partial) = match expanded.rfind('/') {
            Some(at) => (&expanded[..=at], &expanded[at + 1..]),
            // A bare word completes against the conversation's own directory, which is what
            // someone typing `src` means.
            None => ("", expanded.as_str()),
        };
        let base = if dir.is_empty() {
            self.cwd.clone()
        } else {
            std::path::PathBuf::from(dir)
        };

        let read = tokio::task::spawn_blocking({
            let base = base.clone();
            let partial = partial.to_string();
            move || {
                let mut out: Vec<String> = Vec::new();
                let Ok(entries) = std::fs::read_dir(&base) else { return out };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // A leading dot is only offered once you have typed one — otherwise every
                    // completion in a home directory is `.cache`.
                    if name.starts_with('.') && !partial.starts_with('.') {
                        continue;
                    }
                    if !name.starts_with(&partial) {
                        continue;
                    }
                    if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                        continue;
                    }
                    out.push(name);
                    if out.len() > LIMIT * 4 {
                        break;
                    }
                }
                out.sort();
                out.truncate(LIMIT);
                out
            }
        })
        .await
        .unwrap_or_default();

        // Returned as what you would have typed, so a caller can put the answer straight back in
        // the field. The trailing slash is what makes pressing Tab twice descend.
        let paths = read
            .into_iter()
            .map(|name| {
                if dir.is_empty() {
                    format!("{name}/")
                } else {
                    format!("{dir}{name}/")
                }
            })
            .collect();
        Ok(ApiOk::Paths { paths })
    }

    pub async fn git_status(&self) -> ApiResult {
        Ok(ApiOk::Status { status: self.repo()?.status().await.map_err(vcs_err)? })
    }

    pub async fn git_branches(&self, include_remote: bool, cwd: Option<String>) -> ApiResult {
        let repo = self.repo_at(cwd).await?;
        Ok(ApiOk::Branches { branches: repo.branches(include_remote).await.map_err(vcs_err)? })
    }

    pub async fn git_worktrees(&self, cwd: Option<String>) -> ApiResult {
        let repo = self.repo_at(cwd).await?;
        Ok(ApiOk::Worktrees { worktrees: repo.worktrees().await.map_err(vcs_err)? })
    }

    pub async fn git_log(&self, limit: u32) -> ApiResult {
        Ok(ApiOk::Commits { commits: self.repo()?.log(limit).await.map_err(vcs_err)? })
    }

    pub async fn git_diff(&self, target: DiffTarget, stat: bool) -> ApiResult {
        let repo = self.repo()?;
        let text = if stat {
            repo.diff_stat(&target).await
        } else {
            repo.diff(&target).await
        };
        Ok(ApiOk::Text { text: text.map_err(vcs_err)? })
    }

    pub async fn git_default_branch(&self) -> ApiResult {
        Ok(ApiOk::MaybeText { text: self.repo()?.default_branch().await })
    }

    pub async fn git_create_branch(&self, name: String, from: Option<String>) -> ApiResult {
        self.permit_write("switch --create")?;
        self.repo()?.create_branch(&name, from.as_deref()).await.map_err(vcs_err)?;
        Ok(ApiOk::Unit)
    }

    pub async fn git_checkout(&self, rev: String) -> ApiResult {
        self.permit_write("switch")?;
        self.repo()?.checkout(&rev).await.map_err(vcs_err)?;
        Ok(ApiOk::Unit)
    }

    /// `git branch -m`, in whichever checkout the branch belongs to.
    ///
    /// Takes a `cwd` where [`Self::git_checkout`] does not, because the caller this exists for is
    /// renaming the branch of a *worktree* — very often not the one the active conversation is
    /// standing in. Nothing about the working tree changes, so it is safe on a branch that is
    /// checked out and safe while an agent is mid-turn against it.
    pub async fn git_rename_branch(
        &self,
        old: String,
        new: String,
        cwd: Option<String>,
    ) -> ApiResult {
        self.permit_write("branch -m")?;
        let repo = self.repo_at(cwd).await?;
        repo.rename_branch(&old, &new).await.map_err(vcs_err)?;
        Ok(ApiOk::Unit)
    }

    pub async fn git_stage(&self, paths: Vec<String>) -> ApiResult {
        self.permit_write("add")?;
        self.repo()?.stage(&paths).await.map_err(vcs_err)?;
        Ok(ApiOk::Unit)
    }

    pub async fn git_unstage(&self, paths: Vec<String>) -> ApiResult {
        self.permit_write("restore --staged")?;
        self.repo()?.unstage(&paths).await.map_err(vcs_err)?;
        Ok(ApiOk::Unit)
    }

    pub async fn git_commit(&self, message: String) -> ApiResult {
        self.permit_write("commit")?;
        Ok(ApiOk::Commit { commit: self.repo()?.commit(&message).await.map_err(vcs_err)? })
    }

    pub async fn git_add_worktree(
        &self,
        path: String,
        branch: String,
        create: bool,
        cwd: Option<String>,
    ) -> ApiResult {
        self.permit_write("worktree add")?;
        let repo = self.repo_at(cwd).await?;
        repo.add_worktree(&PathBuf::from(path), &branch, create).await.map_err(vcs_err)?;
        Ok(ApiOk::Unit)
    }

    pub async fn git_pull(&self, cwd: Option<String>) -> ApiResult {
        self.permit_write("pull")?;
        let repo = self.repo_at(cwd).await?;
        Ok(ApiOk::Text { text: repo.pull().await.map_err(vcs_err)? })
    }

    pub async fn git_remove_worktree(
        &self,
        path: String,
        force: bool,
        cwd: Option<String>,
    ) -> ApiResult {
        self.permit_write("worktree remove")?;
        let repo = self.repo_at(cwd).await?;
        repo.remove_worktree(&PathBuf::from(path), force).await.map_err(vcs_err)?;
        Ok(ApiOk::Unit)
    }

    // ---- models ----------------------------------------------------------

    /// Every reachable model, paired with the instance that serves it.
    ///
    /// Instances are queried **concurrently**. Sequentially, a machine with five configured
    /// providers waits for the sum of five network round trips before anything appears; the slowest
    /// one should be the only cost.
    pub async fn list_models(&self, only: Option<InstanceId>, refresh: bool) -> ApiResult {
        // Every configured instance, not only the ones with a driver loaded. A plan whose CLI is
        // not installed still has a catalogue, and the list of what it offers is exactly how
        // somebody decides whether installing it is worth their afternoon.
        let targets: Vec<_> = {
            let reg = self.agent.providers();
            reg.instances()
                .filter(|i| only.as_ref().is_none_or(|want| &i.id == want))
                .cloned()
                .collect()
        };

        if refresh {
            let mut cache = self.model_cache.lock().expect("model cache poisoned");
            for inst in &targets {
                cache.remove(&inst.id);
            }
        }

        let lookups = targets.into_iter().map(|inst| {
            let cached = self
                .model_cache
                .lock()
                .expect("model cache poisoned")
                .get(&inst.id)
                .cloned();
            let driver = self.agent.providers().driver(&inst.driver);
            async move {
                if let Some(models) = cached {
                    return (inst.id.clone(), models);
                }
                let models = match driver {
                    // The endpoint decides what exists; the catalogue decides how it reads. Neither
                    // alone is enough — a discovered id with no name or rung is unpickable, and a
                    // written-down list is out of date the week after it is written.
                    Some(d) => match d.list_models(&inst).await {
                        Ok(m) => neosh_provider::catalog::merge(m, &inst.models),
                        Err(e) => {
                            // Falling back to what was configured keeps an offline machine usable —
                            // and, more often, keeps a provider you have not signed into yet
                            // showing what it offers, which is how you decide whether to.
                            tracing::debug!(instance = %inst.id, "model discovery failed: {e}");
                            inst.models.clone()
                        }
                    },
                    None => inst.models.clone(),
                };
                (inst.id.clone(), models)
            }
        });

        let mut out = Vec::new();
        for (instance, models) in futures::future::join_all(lookups).await {
            self.model_cache
                .lock()
                .expect("model cache poisoned")
                .insert(instance.clone(), models.clone());
            out.extend(
                models.into_iter().map(|model| ModelEntry { instance: instance.clone(), model }),
            );
        }
        Ok(ApiOk::Models { models: out })
    }

    // ---- permissions -----------------------------------------------------

    /// Answer a plugin's `neosh.permit`.
    ///
    /// Goes through the same decision point a built-in tool uses, so `ask` mode means the same
    /// thing on both sides of the plugin boundary. Off the host loop, because asking a human takes
    /// as long as it takes.
    // ---- what the week went on ------------------------------------------

    /// Scan the vendor CLIs' transcripts for a span and bucket what is there.
    ///
    /// On a blocking thread, not merely off the host loop: this is synchronous file I/O over
    /// thousands of files somebody else wrote, and a `tokio` worker parked in `read_dir` is a
    /// worker that is not driving anybody's stream.
    ///
    /// Prices come from whatever the model cache already holds, plus the configured catalogue for
    /// anything never discovered. A model with no price still appears — its tokens are counted and
    /// its cost is not — because dropping it would make the busiest week look like the quietest.
    pub async fn usage_history(
        &self,
        since: i64,
        until: i64,
        resolution: neosh_proto::UsageResolution,
        time_zone: Option<String>,
    ) -> ApiResult {
        if until <= since {
            return Err(ApiError::InvalidArgument {
                message: "the span ends before it starts".into(),
            });
        }
        let mut models: Vec<neosh_proto::ModelInfo> = {
            let reg = self.agent.providers();
            reg.instances().flat_map(|i| i.models.iter().cloned()).collect()
        };
        // Discovered models last, so that a live answer's price wins the `HashMap` insert over the
        // written catalogue's — the catalogue is the fallback for a model nobody has asked about.
        {
            let cache = self.model_cache.lock().expect("model cache poisoned");
            models.extend(cache.values().flatten().cloned());
        }
        let prices = crate::usage::price_table(&models);
        let offset = utc_offset(time_zone.as_deref());

        tokio::task::spawn_blocking(move || {
            ApiOk::UsageHistory { history: crate::usage::history(since, until, resolution, offset, &prices) }
        })
        .await
        .map_err(|e| ApiError::Internal { message: format!("usage scan: {e}") })
    }

    pub async fn permission_check(&self, capability: neosh_proto::Capability) -> ApiResult {
        let decision = self.agent.decide(&*self.bridge, capability, None).await;
        Ok(ApiOk::Permission { decision })
    }

    // ---- questions -------------------------------------------------------

    /// Put a question to whoever draws them, and wait.
    ///
    /// Through [`neosh_agent::DriverQuestioner`] rather than a second path of its own, so a
    /// plugin's question and an agent's arrive at the panel identically — which is the test for
    /// whether the API is the real one or a private door beside it.
    pub async fn ask_user(&self, questions: Vec<neosh_proto::UserQuestion>) -> ApiResult {
        use neosh_provider::ask::{QuestionAnswers, QuestionAsker};

        if questions.is_empty() {
            return Err(ApiError::InvalidArgument {
                message: "a question with nothing in it".into(),
            });
        }
        let asker = neosh_agent::DriverQuestioner::new(&self.agent, self.bridge.clone());
        // Read and dropped before the await. The store's guard is not `Send`, and holding one
        // across a question that may sit unanswered for half an hour would lock the conversation
        // list for exactly that long.
        let conversation = self.agent.sessions().active_id().clone();
        let answers = asker
            .ask(neosh_provider::ask::QuestionRequest { questions, conversation, cwd: self.cwd.clone() })
            .await;
        Ok(ApiOk::Answers {
            answers: match answers {
                QuestionAnswers::Answered(a) => Some(a),
                QuestionAnswers::Cancelled { .. } => None,
            },
        })
    }

    // ---- generation ------------------------------------------------------

    pub async fn gen_complete(
        &self,
        prompt: String,
        system: Option<String>,
        json: bool,
        selection: Option<ModelSelection>,
    ) -> ApiResult {
        // Precedence: what the caller asked for, then `gen.model`, then the session's own model.
        // The middle rung is why it lives here: "use a cheap model for branch names" is a setting
        // every generating plugin wants and none should have to reimplement.
        let selection = selection.or_else(|| parse_selection(&self.gen_model));

        let text = self
            .agent
            .complete(prompt, system, selection, CancellationToken::new())
            .await
            .map_err(|message| ApiError::Internal { message })?;

        if !json {
            return Ok(ApiOk::Text { text });
        }
        match extract_json(&text) {
            Some(value) => Ok(ApiOk::Json { value }),
            None => Err(ApiError::Internal {
                message: format!("expected JSON, got: {}", truncate(&text, 200)),
            }),
        }
    }
}

fn vcs_err(e: VcsError) -> ApiError {
    match e {
        VcsError::NotARepository(p) => ApiError::NotFound { what: format!("git repository at {p}") },
        VcsError::GitMissing => ApiError::NotFound { what: "git on PATH".into() },
        other => ApiError::Internal { message: other.to_string() },
    }
}

/// `instance/model`, or `None` for anything that is not exactly that.
fn parse_selection(spec: &str) -> Option<ModelSelection> {
    let spec = spec.trim();
    let (instance, model) = spec.split_once('/')?;
    if instance.is_empty() || model.is_empty() {
        return None;
    }
    Some(ModelSelection {
        instance: neosh_proto::InstanceId(instance.to_string()),
        model: neosh_proto::ModelId(model.to_string()),
        options: Vec::new(),
    })
}

/// Pull a JSON value out of whatever a model actually returned.
///
/// Asking for JSON gets you JSON *most* of the time. The rest of the time it is fenced in
/// ```` ```json ````, prefixed with "Here you go:", or both. Every caller would otherwise write
/// this, and write it slightly differently.
pub fn extract_json(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str(trimmed) {
        return Some(v);
    }

    // A fenced block, with or without a language tag.
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let body = match after.find('\n') {
            Some(nl) => &after[nl + 1..],
            None => after,
        };
        if let Some(end) = body.find("```")
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(body[..end].trim())
        {
            return Some(v);
        }
    }

    // Last resort: scan for a balanced bracket pair that parses.
    //
    // Not "first `{` to last `}`" — prose like "use {placeholder} syntax" ahead of the real object
    // makes that span unparseable. Not "the first balanced pair" either, for the same reason: the
    // first pair *is* `{placeholder}`. So each candidate start is tried in turn, and the first one
    // that both balances and parses wins.
    let bytes = trimmed.as_bytes();
    for start in 0..bytes.len() {
        let open = bytes[start];
        if open != b'{' && open != b'[' {
            continue;
        }
        let close = if open == b'{' { b'}' } else { b']' };
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        for (i, &b) in bytes.iter().enumerate().skip(start) {
            if escaped {
                escaped = false;
                continue;
            }
            match b {
                b'\\' if in_string => escaped = true,
                b'"' => in_string = !in_string,
                _ if in_string => {}
                b if b == open => depth += 1,
                b if b == close => {
                    depth -= 1;
                    if depth == 0 {
                        if let Ok(v) = serde_json::from_str(&trimmed[start..=i]) {
                            return Some(v);
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    // Byte slicing a model's output would panic on a multi-byte boundary, which is a spectacular
    // way to crash while reporting a different error.
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_json_parses() {
        let v = extract_json(r#"{"branch": "fix-login"}"#).expect("parses");
        assert_eq!(v["branch"], "fix-login");
    }

    #[test]
    fn fenced_json_parses() {
        let v = extract_json("```json\n{\"branch\": \"fix-login\"}\n```").expect("parses");
        assert_eq!(v["branch"], "fix-login");
        let v = extract_json("```\n{\"a\": 1}\n```").expect("unlabelled fence parses");
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn json_after_a_preamble_parses() {
        let v = extract_json("Sure! Here is the branch name:\n\n{\"branch\": \"fix-login\"}")
            .expect("parses");
        assert_eq!(v["branch"], "fix-login");
    }

    #[test]
    fn a_brace_in_prose_does_not_swallow_the_object() {
        // The naive "first { to last }" approach returns the whole string here and fails to parse.
        let v = extract_json("Use {placeholder} syntax.\n{\"branch\": \"x\"}").expect("parses");
        assert_eq!(v["branch"], "x");
    }

    #[test]
    fn braces_inside_strings_do_not_end_the_object() {
        let v = extract_json(r#"{"subject": "fix the {thing}", "body": ""}"#).expect("parses");
        assert_eq!(v["subject"], "fix the {thing}");
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        let v = extract_json(r#"{"subject": "say \"hi\" }", "n": 1}"#).expect("parses");
        assert_eq!(v["subject"], r#"say "hi" }"#);
        assert_eq!(v["n"], 1);
    }

    #[test]
    fn arrays_parse_too() {
        let v = extract_json("[\"a\", \"b\"]").expect("parses");
        assert_eq!(v[1], "b");
    }

    #[test]
    fn prose_with_no_json_is_none() {
        assert!(extract_json("I'm sorry, I can't do that.").is_none());
        assert!(extract_json("").is_none());
    }

    #[test]
    fn selection_needs_both_halves() {
        assert!(parse_selection("").is_none());
        assert!(parse_selection("anthropic").is_none());
        assert!(parse_selection("/opus").is_none());
        assert!(parse_selection("anthropic/").is_none());
        let s = parse_selection("anthropic/claude-opus-5").expect("parses");
        assert_eq!(s.instance.0, "anthropic");
        assert_eq!(s.model.0, "claude-opus-5");
    }

    #[test]
    fn truncate_does_not_split_a_character() {
        // A byte slice here would panic; the point of the test is that it does not.
        let s = "日本語".repeat(100);
        assert!(truncate(&s, 5).chars().count() <= 6);
        assert_eq!(truncate("short", 200), "short");
    }
}

/// `~` and `~/…` against `$HOME`.
///
/// Done here rather than in the plugin because the runtime has no environment to read. A bare `~`
/// with no home set is left alone rather than becoming `/`, which would silently complete against
/// the root of the filesystem.
fn expand_home(path: &str) -> String {
    with_home(path, std::env::var("HOME").ok().as_deref())
}

/// The substitution itself, with the home directory passed in.
///
/// Split out so the tests do not have to set an environment variable: `set_var` is unsafe in this
/// edition for a good reason, and a test that mutates the process environment while other tests
/// read it is a race that surfaces weeks later as something else entirely.
fn with_home(path: &str, home: Option<&str>) -> String {
    let Some(rest) = path.strip_prefix('~') else { return path.to_string() };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return path.to_string();
    }
    match home {
        Some(home) => format!("{home}{rest}"),
        None => path.to_string(),
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    fn home(path: &str) -> String {
        with_home(path, Some("/home/someone"))
    }

    #[test]
    fn a_tilde_becomes_the_home_directory() {
        assert_eq!(home("~"), "/home/someone");
        assert_eq!(home("~/src"), "/home/someone/src");
    }

    #[test]
    fn a_tilde_that_is_part_of_a_name_is_left_alone() {
        // `~backup` is a real directory name, and `~user` is a shell expansion we do not implement.
        // Rewriting either would complete against somewhere the user did not ask for.
        assert_eq!(home("~backup"), "~backup");
        assert_eq!(home("/tmp/~x"), "/tmp/~x");
    }

    #[test]
    fn with_no_home_set_a_tilde_is_left_alone_rather_than_becoming_the_root() {
        assert_eq!(with_home("~/src", None), "~/src");
    }
}

/// Seconds this zone is ahead of UTC, right now.
///
/// Named zones are resolved through `TZ`, which is the only lookup available without a tzdata
/// crate — and it is enough, because the caller that matters is a terminal on the machine whose
/// clock this is. An unknown zone falls back to the machine's own offset rather than to UTC: being
/// an hour out is a chart one column wrong, and defaulting to UTC is a chart that is wrong for
/// everybody who is not in London.
pub fn utc_offset(time_zone: Option<&str>) -> i64 {
    match time_zone {
        // The caller asked for the two things that need no lookup at all.
        Some("UTC") | Some("Etc/UTC") | Some("Z") => 0,
        _ => local_offset(),
    }
}

/// What `localtime` makes of now, minus `gmtime` of the same instant.
///
/// Derived from the difference rather than read out of a field, because that is the one thing the
/// standard library will tell us without a dependency: formatting the same instant twice and
/// subtracting is exact, including through a DST change.
fn local_offset() -> i64 {
    use std::process::Command;
    // `date` is on every unix and answers in seconds of offset directly. A failure here is an
    // offset of zero, which is a chart bucketed in UTC — visibly a little off for some, rather
    // than absent.
    let out = Command::new("date").arg("+%z").output().ok();
    let Some(out) = out.filter(|o| o.status.success()) else { return 0 };
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if text.len() < 5 {
        return 0;
    }
    let sign = if text.starts_with('-') { -1 } else { 1 };
    let hours: i64 = text[1..3].parse().unwrap_or(0);
    let minutes: i64 = text[3..5].parse().unwrap_or(0);
    sign * (hours * 3_600 + minutes * 60)
}

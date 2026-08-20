//! `neosh` — a terminal-first agent workspace.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use neosh_agent::{Agent, PermissionLayer, Session};
use neosh_proto::{InputEvent, ModelId};
use neosh_provider::ProviderRegistry;
use neosh_script::ScriptRuntime;

mod markdown;
mod bridge;
mod config;
mod frontend;
mod host;
mod paths;
mod pstate;
mod scaffold;
mod services;
mod sessions;
mod trust;

use bridge::ScriptBridge;
use frontend::{Disconnected, Frontend, StdioFrontend, TerminalUi};
use host::Host;
use paths::Paths;

#[derive(Copy, Clone, PartialEq, Eq, Debug, ValueEnum)]
enum UiProtocol {
    /// Render to this terminal.
    Terminal,
    /// Emit the UI event protocol as JSON lines on stdout and read input as JSON lines on stdin.
    ///
    /// The same events the terminal renders. This is what a web or mobile frontend would consume,
    /// and what the end-to-end tests assert on.
    Stdio,
}

#[derive(Parser, Debug)]
#[command(name = "neosh", version, about = "A terminal-first agent workspace")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    #[arg(long, value_enum, default_value_t = UiProtocol::Terminal)]
    ui_protocol: UiProtocol,

    /// Model to start with, as `instance/model` (e.g. `anthropic/claude-opus-5`).
    #[arg(long)]
    model: Option<String>,

    /// Extra directory to load plugins from. Repeatable.
    #[arg(long = "plugin-dir")]
    plugin_dirs: Vec<PathBuf>,

    /// Workspace root. Defaults to the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Replay a recorded provider stream instead of calling a real model.
    ///
    /// One JSON-encoded `ProviderEvent` per line; a blank line separates turns. Lets the whole
    /// stack run in CI with no API key and no network.
    #[arg(long)]
    mock_script: Option<PathBuf>,

    /// Print every model neosh can reach, and exit.
    #[arg(long)]
    list_models: bool,

    /// Use this directory instead of the usual config location.
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Start with no config, no plugins and no trust decisions. The bug-report flag.
    #[arg(long)]
    clean: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Write a starter config: `init.ts`, `config.toml`, and types for your editor.
    Init(InitArgs),
    /// Allow this project's `.neosh/` to run code.
    Trust(TrustArgs),
    /// Print where neosh reads and writes, and whether each of those exists.
    Paths,
}

#[derive(Args, Debug)]
struct InitArgs {
    /// Scaffold `.neosh/` in the current directory instead of your config directory.
    #[arg(long)]
    project: bool,
    /// Overwrite files you already wrote.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct TrustArgs {
    /// Withdraw trust from this project.
    #[arg(long)]
    revoke: bool,
    /// Print every trusted project.
    #[arg(long)]
    list: bool,
}

/// `println!` for command output, which does not panic when the reader goes away.
///
/// `println!` panics on `EPIPE`, so `neosh --list-models | head` produces a crash report instead of
/// exiting the way every other unix tool does. This is only for the one-shot subcommands; the UI
/// protocol has its own writer.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            // The reader closed the pipe. That is not an error, it is `head`.
            std::process::exit(0);
        }
    }};
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Logs go to a file, never stdout: stdout is the UI protocol, and a stray log line would
    // corrupt the stream a frontend is parsing.
    let log_path = std::env::var_os("NEOSH_LOG").map(PathBuf::from);
    let _guard = init_logging(log_path.as_deref());

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(run(cli));

    // Abandon the runtime rather than dropping it. `tokio::io::stdin()` reads on a blocking task,
    // and dropping a runtime *waits* for blocking tasks — so a frontend that holds our stdin open
    // (which is every frontend, right up until it decides we have exited) would keep the process
    // alive forever after a clean shutdown. Everything needing ordering — the last flush, plugin
    // teardown, restoring the terminal — has already happened by the time `run` returns.
    runtime.shutdown_background();
    result
}

fn init_logging(path: Option<&std::path::Path>) -> Option<std::fs::File> {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("NEOSH_LOG_LEVEL").unwrap_or_else(|_| EnvFilter::new("warn"));
    match path.and_then(|p| std::fs::File::create(p).ok()) {
        Some(file) => {
            let clone = file.try_clone().ok();
            tracing_subscriber::fmt().with_env_filter(filter).with_writer(move || {
                clone.as_ref().and_then(|f| f.try_clone().ok()).expect("log file")
            })
            .with_ansi(false)
            .init();
            Some(file)
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
            None
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let paths = Paths::resolve(cli.config_dir.clone(), cli.clean);
    // Canonicalised, because it is displayed and grouped on: `--cwd .` is a perfectly ordinary
    // thing to type, and a project called "." tells the user nothing. Falls back to the literal
    // path if the directory does not resolve, so a bad `--cwd` still reaches its own error.
    let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);

    match &cli.command {
        Some(Cmd::Init(args)) => return run_init(&paths, &cwd, args),
        Some(Cmd::Trust(args)) => return run_trust(&paths, &cwd, args),
        Some(Cmd::Paths) => return run_paths(&paths, &cwd),
        None => {}
    }

    // Keep the config directory's API types in step with this binary, so `tsc` in someone's
    // config directory checks against the neosh they are actually running.
    config::refresh_api_types(&paths);

    let trust = config::trust_store(&paths).status(&cwd);
    let resolved = config::resolve(&paths, &cwd, trust)?;

    let permissions = {
        let mut p = PermissionLayer::new(&cwd).with_mode(resolved.permissions.mode);
        for c in &resolved.permissions.allow_commands {
            p = p.allow_command(c);
        }
        for h in &resolved.permissions.allow_hosts {
            p = p.allow_host(h);
        }
        for r in &resolved.permissions.allow_roots {
            p = p.allow_root(r);
        }
        Arc::new(p)
    };

    let mut session = Session::new(&cwd);
    session.system = Some(default_system_prompt());
    // Stamped here rather than inside `Session::new`, which stays clock-free so the store is
    // deterministic to test. Without this the conversation you start in has no age and sorts last
    // in a list ordered by when things happened.
    session.created_at = host::now_secs();
    session.updated_at = session.created_at;

    let (agent, agent_rx) = Agent::new(session, permissions, ProviderRegistry::new());
    let agent = Arc::new(agent);
    agent.tools_mut().register_builtin(Arc::new(neosh_agent::tools::ReadFile));

    let (script, script_rx) = ScriptRuntime::spawn();
    let bridge = Arc::new(ScriptBridge::new(script));

    match &cli.mock_script {
        Some(path) => install_mock_provider(&agent, path)?,
        None => {
            host::install_builtin_providers(&agent);
            for i in resolved.providers.clone() {
                agent.providers_mut().add_instance(i);
            }
        }
    }

    // Before any frontend: listing models must work in a pipe, a script, or CI.
    if cli.list_models {
        return list_models(&agent).await;
    }

    let (frontend, input_rx): (Box<dyn Frontend>, _) = match cli.ui_protocol {
        UiProtocol::Terminal => {
            let (f, rx) = TerminalUi::enter().map_err(|e| {
                anyhow::anyhow!(
                    "could not take over the terminal ({e}). If this is not an interactive \
                     terminal, use --ui-protocol=stdio."
                )
            })?;
            (Box::new(f), rx)
        }
        UiProtocol::Stdio => {
            let (f, rx) = StdioFrontend::new();
            (Box::new(f), rx)
        }
    };

    // Signals join the same input stream as keys, so a `kill` leaves through the ordinary quit
    // path — flushing, tearing down plugins, and restoring the terminal — rather than dropping the
    // process with the alternate screen still on.
    let input_rx = with_signals(input_rx);

    let mut host = Host::new(agent.clone(), bridge.clone(), frontend);
    host.discover_repo(&cwd).await;
    // `--clean` reads and writes nothing, which has to include conversations.
    host.restore_sessions((!paths.clean).then(|| paths.state.clone())).await;
    // Everything from here — options, config scripts, plugin discovery, model selection — is
    // sequenced by the host, because config activation is asynchronous and the steps depend on it.
    //
    // The reload closure re-reads every layer, including the trust store: a project trusted since
    // launch takes effect on reload rather than needing a restart.
    let (rpaths, rcwd) = (paths.clone(), cwd.clone());
    host.begin_startup(host::Bootstrap {
        resolved,
        reload: Box::new(move || {
            let trust = config::trust_store(&rpaths).status(&rcwd);
            config::resolve(&rpaths, &rcwd, trust)
        }),
        cli_model: cli.model.clone(),
        cli_plugin_dirs: cli.plugin_dirs.clone(),
    });

    match host.run(script_rx, agent_rx, input_rx).await {
        // The consumer closing the pipe ends the session; it is not something to report.
        Err(e) if e.downcast_ref::<Disconnected>().is_some() => Ok(()),
        other => other,
    }
}

/// Forward frontend input, and turn SIGINT/SIGTERM into the same `quit` command the key runs.
///
/// Raw mode disables ISIG, so an interactive `<C-c>` arrives as a key rather than a signal — these
/// are the *other* ways a session ends: `kill`, a closing SSH connection, a supervisor stopping the
/// service. Reusing the command rather than exiting directly means shutdown has exactly one path.
fn with_signals(
    mut from_frontend: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
) -> tokio::sync::mpsc::UnboundedReceiver<InputEvent> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    let forward = tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = from_frontend.recv().await {
            if forward.send(ev).is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let quit = InputEvent::Command { name: "quit".into(), args: Vec::new() };
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            // A failure to install a handler is not worth refusing to start over; it only means
            // this particular way of leaving is unavailable.
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("cannot handle SIGTERM: {e}");
                    return;
                }
            };
            let mut hup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("cannot handle SIGHUP: {e}");
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = tx.send(quit);
    });

    rx
}

fn run_init(paths: &Paths, cwd: &std::path::Path, args: &InitArgs) -> anyhow::Result<()> {
    let report = if args.project {
        scaffold::init_project(cwd, args.force)?
    } else {
        scaffold::init_user(paths, args.force)?
    };
    for p in &report.written {
        say!("wrote   {}", p.display());
    }
    for p in &report.kept {
        say!("kept    {} (--force to replace)", p.display());
    }
    if args.project {
        say!("\nEdit .neosh/config.toml, then `neosh trust` to let it run code.");
    } else {
        say!(
            "\nEdit {}. Run `npx tsc --noEmit` there for type checking.",
            paths.init_script().display()
        );
    }
    Ok(())
}

/// Where everything lives, and what is actually there.
///
/// The first question of any config problem — and of any bug report — is "which files did you
/// read?". Neovim answers it with `stdpath()`; without an equivalent the only way to find out is to
/// read this source.
fn run_paths(paths: &Paths, cwd: &std::path::Path) -> anyhow::Result<()> {
    fn row(label: &str, path: &std::path::Path, note: &str) {
        let mark = if path.exists() { "" } else { "  (does not exist)" };
        say!("{label:<9} {}{mark}{note}", path.display());
    }

    row("config", &paths.config, "");
    row("  init", &paths.init_script(), "   ← your configuration");
    row("  toml", &paths.config_file(), "");
    row("  plugins", &paths.plugin_dir(), "");
    row("data", &paths.data, "");
    row("state", &paths.state, "");
    row("cache", &paths.cache, "");

    let project = Paths::project_dir(cwd);
    let status = crate::config::trust_store(paths).status(cwd);
    let note = match status {
        trust::Status::NoProjectConfig => "",
        trust::Status::Trusted => "   trusted",
        trust::Status::Untrusted => "   untrusted — run `neosh trust`",
        trust::Status::Stale => "   changed since trusted — review, then `neosh trust`",
    };
    row("project", &project, note);

    if paths.clean {
        say!("\n--clean: none of the above is read or written this run.");
    }
    Ok(())
}

fn run_trust(paths: &Paths, cwd: &std::path::Path, args: &TrustArgs) -> anyhow::Result<()> {
    let mut store = config::trust_store(paths);
    if args.list {
        let mut any = false;
        for p in store.list() {
            any = true;
            say!("{p}");
        }
        if !any {
            say!("no trusted projects");
        }
        return Ok(());
    }
    if args.revoke {
        if store.revoke(cwd)? {
            say!("revoked trust for {}", cwd.display());
        } else {
            say!("{} was not trusted", cwd.display());
        }
        return Ok(());
    }

    let dir = Paths::project_dir(cwd);
    // Show what is being approved. "Trust this?" with nothing on screen is a rubber stamp — and
    // trust covers every file under `.neosh/`, not just the two entry points, so every file has to
    // be on screen.
    let mut files = Vec::new();
    trust::list_covered(cwd, &mut files);
    files.sort();
    for rel in &files {
        let f = dir.join(rel);
        match std::fs::read_to_string(&f) {
            Ok(text) => {
                say!("--- {} ---", f.display());
                say!("{}", text.trim_end());
            }
            // Binary or unreadable: name it rather than pretending it is not covered.
            Err(_) => say!("--- {} --- (not shown)", f.display()),
        }
    }
    store.trust(cwd)?;
    say!("\ntrusted {}", dir.display());
    say!("Editing these files revokes trust; run `neosh trust` again after reviewing.");
    Ok(())
}

fn default_system_prompt() -> String {
    "You are the agent inside neosh, a terminal-first coding workspace. Be concise. Use the tools \
     available to you rather than guessing about the contents of files."
        .to_string()
}

fn install_mock_provider(agent: &Agent, path: &std::path::Path) -> anyhow::Result<()> {
    use neosh_provider::drivers::MockProvider;
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    // A blank line separates turns, so one fixture can drive a whole tool loop.
    let turns: Vec<Vec<neosh_proto::ProviderEvent>> = text
        .split("\n\n")
        .filter(|c| !c.trim().is_empty())
        .map(MockProvider::from_jsonl)
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;

    let mut reg = agent.providers_mut();
    reg.register_driver(Arc::new(MockProvider::new(turns)));
    reg.add_instance(neosh_proto::InstanceConfig {
        id: "mock".into(),
        driver: "mock".into(),
        display_name: "Mock".into(),
        base_url: None,
        auth: neosh_proto::AuthRef::None,
        brand: None,
        models: vec![neosh_proto::ModelInfo::undescribed(ModelId::from("mock"), "Mock")],
        extra_headers: vec![],
    });
    Ok(())
}

async fn list_models(agent: &Agent) -> anyhow::Result<()> {
    let instances: Vec<_> = agent.providers().usable_instances().cloned().collect();
    for inst in instances {
        let driver = agent.providers().driver(&inst.driver);
        let models = match &driver {
            Some(d) => d.list_models(&inst).await.unwrap_or_else(|_| inst.models.clone()),
            None => inst.models.clone(),
        };
        // What the credential store says, not what the config file says: a key typed in last week
        // and put in the keychain is why an instance whose environment variable is unset works
        // anyway, and a listing that reported otherwise would send people looking for a bug.
        let source = neosh_provider::credentials::credentials().source(&inst);
        let note = match source {
            neosh_proto::CredentialSource::NotNeeded => String::new(),
            other => format!("  ({})", other.summary(&inst.auth)),
        };
        say!("{} — {}{}", inst.id, inst.display_name, note);
        if models.is_empty() {
            // Why there is nothing, not just that there is nothing. The two causes need opposite
            // fixes: one is a key, the other is starting the server.
            match &inst.auth {
                neosh_proto::AuthRef::None => say!(
                    "    (nothing reachable at {})",
                    inst.base_url.as_deref().unwrap_or("the configured address")
                ),
                _ => say!("    (no models listed \u{2014} discovery needs a key)"),
            }
        }
        for m in models {
            say!("    {}/{}", inst.id, m.id);
        }
    }
    Ok(())
}

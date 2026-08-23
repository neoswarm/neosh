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
mod client;
mod daemon;
mod cards;
mod diff;
mod logo;
mod config;
mod images;
mod frontend;
mod host;
mod paths;
mod plugins;
mod pstate;
mod scaffold;
mod services;
mod sessions;
mod quota;
mod swarm;
mod trust;
mod usage;
mod vars;
mod vim;
mod views;

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

    /// Be the workspace: hold the sessions, the plugins and the turns, and serve terminals.
    ///
    /// Started for you the first time you run `neosh`, so this is here for watching one boot when
    /// it will not — run it in a terminal and the errors that were going to `/dev/null` arrive on
    /// screen instead.
    #[arg(long)]
    serve: bool,

    /// Do everything in this process, as neosh did before there was a workspace to attach to.
    ///
    /// Nothing survives the terminal closing. It is what `--ui-protocol=stdio` needs — a pipe has
    /// no one to hand a running workspace to — and what a test that wants one process rather than
    /// two needs.
    #[arg(long)]
    no_daemon: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Say what the workspace is holding: how long it has been up, and what is running in it.
    Status,
    /// Stop the workspace, and everything running in it.
    ///
    /// Closing a terminal does not do this, which is the point of the workspace being a process of
    /// its own. This is how you say you meant it.
    Stop,
    /// Write a starter config: `init.ts`, `config.toml`, and types for your editor.
    Init(InitArgs),
    /// Allow this project's `.neosh/` to run code.
    Trust(TrustArgs),
    /// Print where neosh reads and writes, and whether each of those exists.
    Paths,
    /// Install, list, update and remove plugins.
    ///
    /// A plugin arrives as a git clone, because a URL is already a globally unique name that works
    /// for a fork, a private repository and the branch you are writing. Bundled and hand-managed
    /// plugins are listed too — what loads is one question, and it should have one answer.
    #[command(subcommand)]
    Plugin(PluginCmd),
    /// Print this machine's swarm identity, and the line to paste on the other computers.
    ///
    /// A node is named by its public key, so joining two machines means each one having the
    /// other's. There is no way to read a public key out of the private one by looking at it, and
    /// a swarm you cannot join without writing a script is a swarm nobody joins.
    SwarmId,
}

#[derive(Subcommand, Debug)]
enum PluginCmd {
    /// Clone a plugin and make it load next time.
    Add {
        /// Anything `git clone` takes: an https URL, an `scp`-style SSH remote, a local bare repo.
        source: String,
    },
    /// Every plugin this machine would load, and where each came from.
    List,
    /// Pull an installed plugin, or all of them.
    Update {
        /// The plugin's name from its manifest. Omit for all of them.
        name: Option<String>,
    },
    /// Delete an installed plugin.
    ///
    /// Irreversible, so it asks — unless `--yes`, or unless nobody is there to answer, in which
    /// case it refuses rather than guessing.
    Remove {
        name: String,
        /// Do not ask.
        #[arg(long, short = 'y')]
        yes: bool,
    },
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
        Some(Cmd::Plugin(cmd)) => return run_plugin(&paths, cmd),
        Some(Cmd::SwarmId) => return run_swarm_id(&paths),
        Some(Cmd::Status) => return run_status(&paths).await,
        Some(Cmd::Stop) => return run_stop(&paths).await,
        None => {}
    }

    // The ordinary case: be a terminal looking at a workspace, starting one if there is not one
    // already. Everything below this — config, plugins, the agent — belongs to the workspace, and
    // a client that built any of it would be paying for a second copy it never uses.
    //
    // `--clean` stays in-process on purpose. It means "read and write nothing", and attaching to a
    // workspace that has been running with somebody's real config is the opposite of that.
    if !cli.serve && !cli.no_daemon && !cli.clean && cli.ui_protocol == UiProtocol::Terminal
        && cli.mock_script.is_none()
        && !cli.list_models
    {
        return run_client(&cli, &paths, &cwd).await;
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

    let mut agent_drivers = Vec::new();
    match &cli.mock_script {
        Some(path) => install_mock_provider(&agent, path)?,
        None => {
            agent_drivers = host::install_builtin_providers(&agent);
            for i in resolved.providers.clone() {
                agent.providers_mut().add_instance(i);
            }
        }
    }

    // Before any frontend: listing models must work in a pipe, a script, or CI.
    if cli.list_models {
        return list_models(&agent).await;
    }

    // Serving a terminal rather than being one. Bound before anything expensive so a second
    // workspace fails immediately with a sentence rather than after ten seconds of loading
    // plugins it is about to throw away.
    let mut serving = match cli.serve {
        true => Some(daemon::Daemon::bind(&paths.socket()).await?),
        false => None,
    };

    let (frontend, input_rx): (Box<dyn Frontend>, _) = match (&mut serving, cli.ui_protocol) {
        (Some(d), _) => d.wire(),
        (None, UiProtocol::Terminal) => {
            let (f, rx) = TerminalUi::enter().map_err(|e| {
                anyhow::anyhow!(
                    "could not take over the terminal ({e}). If this is not an interactive \
                     terminal, use --ui-protocol=stdio."
                )
            })?;
            (Box::new(f), from_the_only_view(rx))
        }
        (None, UiProtocol::Stdio) => {
            let (f, rx) = StdioFrontend::new();
            (Box::new(f), from_the_only_view(rx))
        }
    };

    // Signals join the same input stream as keys, so a `kill` leaves through the ordinary quit
    // path — flushing, tearing down plugins, and restoring the terminal — rather than dropping the
    // process with the alternate screen still on.
    let input_rx = with_signals(input_rx);

    let mut host = Host::new(agent.clone(), bridge.clone(), frontend);
    if let Some(d) = &serving {
        host.serve(d.live());
    }
    host.adopt_agent_drivers(agent_drivers);
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

    // Accepting terminals runs alongside the host, not instead of it: clients come and go while
    // the run loop below never learns that anybody did.
    let socket = serving.as_ref().map(|_| paths.socket());
    if let Some(d) = serving {
        tokio::spawn(d.serve());
    }

    let result = match host.run(script_rx, agent_rx, input_rx).await {
        // The consumer closing the pipe ends the session; it is not something to report.
        Err(e) if e.downcast_ref::<Disconnected>().is_some() => Ok(()),
        other => other,
    };
    // A workspace that is gone should not leave something behind for the next `neosh` to try to
    // connect to. It would be removed by whoever binds next anyway; doing it here means `neosh
    // status` says "no workspace" straight away rather than after the next start.
    if let Some(path) = socket {
        daemon::Daemon::cleanup(&path);
    }
    result
}

/// Tag a lone frontend's input as coming from the one view there is.
///
/// A process that *is* its own terminal has exactly one view and it never goes away, so the tag is
/// a constant. It exists so that "which terminal was that key pressed in" has the same answer
/// everywhere rather than being a question only the daemon build knows how to ask.
fn from_the_only_view(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
) -> tokio::sync::mpsc::UnboundedReceiver<(views::ViewId, InputEvent)> {
    let (tx, tagged) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if tx.send((views::ViewId::LOCAL, ev)).is_err() {
                break;
            }
        }
    });
    tagged
}

/// Forward frontend input, and turn SIGINT/SIGTERM into the same `quit` command the key runs.
///
/// Raw mode disables ISIG, so an interactive `<C-c>` arrives as a key rather than a signal — these
/// are the *other* ways a session ends: `kill`, a closing SSH connection, a supervisor stopping the
/// service. Reusing the command rather than exiting directly means shutdown has exactly one path.
fn with_signals(
    mut from_frontend: tokio::sync::mpsc::UnboundedReceiver<(views::ViewId, InputEvent)>,
) -> tokio::sync::mpsc::UnboundedReceiver<(views::ViewId, InputEvent)> {
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
        // `stop` rather than `quit`, because a signal is a reason to stop the work and not to
        // close one of somebody's windows — and in a served workspace `quit` is exactly "close
        // one of somebody's windows". Tagged with the process's own view, which in a workspace
        // names nobody: `quit` therefore detached a view that was not there, so a `kill` on a
        // workspace did nothing at all and the `SIGKILL` behind it was what actually stopped it,
        // with whatever had not been written to disk still in memory. This is also the path a
        // machine shutting down takes.
        let quit = (views::ViewId::LOCAL, InputEvent::Command { name: "stop".into(), args: Vec::new() });
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

/// Be a terminal looking at a workspace, starting one if there is not one already.
///
/// The whole of the client is here and in [`crate::client`]: connect, draw, send keys. It builds no
/// config, loads no plugins and holds no conversation — which is why a second terminal costs a
/// socket rather than a second copy of everything.
async fn run_client(cli: &Cli, paths: &Paths, cwd: &std::path::Path) -> anyhow::Result<()> {
    let socket = paths.socket();
    let args = client::workspace_args(
        &cli.config_dir,
        cwd,
        &cli.model,
        &cli.plugin_dirs,
        &cli.mock_script,
    );
    let stream = client::connect_or_start(&socket, &args).await?;
    match client::attach(stream).await? {
        // Nothing to say. The terminal is back and the work carries on without it, which is what
        // was asked for — announcing it every time would be a program congratulating itself.
        client::Ended::Detached(neosh_proto::DetachReason::Asked) => {}
        client::Ended::Detached(neosh_proto::DetachReason::TakenOver) => {
            say!("neosh: this workspace is now open in another terminal.");
        }
        client::Ended::Detached(neosh_proto::DetachReason::Stopping)
        | client::Ended::Stopped => {}
    }
    Ok(())
}

/// What the workspace is holding, without disturbing it.
async fn run_status(paths: &Paths) -> anyhow::Result<()> {
    let socket = paths.socket();
    let Some(s) = client::status(&socket).await? else {
        say!("no workspace running ({})", socket.display());
        return Ok(());
    };
    let word = |n: usize, one: &str| if n == 1 { one.to_string() } else { format!("{one}s") };
    say!(
        "running  ·  up {}  ·  {} {}  ·  {}",
        friendly(s.uptime_secs),
        s.conversations,
        word(s.conversations, "conversation"),
        if s.attached { "a terminal is attached".to_string() } else { format!("nobody watching for {}", friendly(s.idle_secs)) }
    );
    if s.running.is_empty() {
        say!("nothing running");
        return Ok(());
    }
    say!("");
    for t in &s.running {
        say!("  {:>8}  {}  ({})", friendly(t.elapsed_secs), t.label, t.cwd);
    }
    Ok(())
}

/// A duration a person would say out loud.
fn friendly(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

async fn run_stop(paths: &Paths) -> anyhow::Result<()> {
    let socket = paths.socket();
    // Said before stopping, because whatever is running is about to be. Nobody should discover
    // afterwards that they killed a turn they would have waited for.
    if let Some(s) = client::status(&socket).await?
        && !s.running.is_empty()
    {
        for t in &s.running {
            say!("stopping after {}: {}", friendly(t.elapsed_secs), t.label);
        }
    }
    match client::stop(&socket).await? {
        true => say!("stopped"),
        false => say!("no workspace running ({})", socket.display()),
    }
    Ok(())
}

/// Show this node's id, and the config a peer needs.
///
/// Creates the key if there is not one yet, so the first thing a person does when setting up a
/// swarm produces the thing they have to send to the other machine — rather than telling them to
/// start neosh first and then come back.
fn run_swarm_id(paths: &Paths) -> anyhow::Result<()> {
    let path = neosh_swarm::identity::key_path(&paths.state);
    let identity = neosh_swarm::Identity::load_or_create(&path)?;
    say!("{}", identity.id());
    say!("");
    say!("On every other computer, put this in `[swarm]`:");
    say!("");
    say!("  [[swarm.peers]]");
    say!("  addr = \"{}:7717\"   # whatever your network calls this machine", machine_name());
    say!("  id   = \"{}\"", identity.id());
    say!("  name = \"{}\"", machine_name());
    Ok(())
}

/// What this computer is called, for the sample config above.
fn machine_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .map(|h| h.split('.').next().unwrap_or(&h).to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this-machine".to_string())
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
/// `neosh plugin ...`.
///
/// Every one of these prints what it did rather than exiting quietly. Installing something that
/// runs in your editor is exactly the moment to say what it is called, what version it is, and what
/// it declared it may do — the permissions list is enforced, so it is a promise the user can hold
/// neosh to rather than a description.
fn run_plugin(paths: &Paths, cmd: &PluginCmd) -> anyhow::Result<()> {
    match cmd {
        PluginCmd::Add { source } => {
            let m = plugins::add(paths, source)?;
            say!("installed {} {}", m.name, m.version);
            if m.permissions.is_empty() {
                say!("  it declared no permissions: no tools, no providers, no blocking hooks.");
            } else {
                let names: Vec<String> =
                    m.permissions.iter().map(|p| permission_name(*p)).collect();
                say!("  it may: {}", names.join(", "));
            }
            say!("\nIt loads when a workspace next starts. `neosh stop` ends the running one.");
        }
        PluginCmd::List => {
            let all = plugins::list(paths);
            if all.is_empty() {
                say!("no plugins.");
                return Ok(());
            }
            for e in &all {
                let perms: Vec<String> =
                    e.permissions.iter().map(|p| permission_name(*p)).collect();
                let perms =
                    if perms.is_empty() { String::new() } else { format!("  [{}]", perms.join(" ")) };
                say!("{:<20} {:<10} {}{}", e.name, e.version, e.origin.label(), perms);
                // Where it is, and where it came from. A bundled one has neither and needs
                // neither; for everything else these are the two questions somebody reading this
                // list is about to ask — which copy is loading, and whether `update` can move it.
                if let Some(d) = &e.dir {
                    say!("{:<20} {}", "", d.display());
                }
                if let Some(r) = &e.remote {
                    say!("{:<20} {r}", "");
                }
            }
        }
        PluginCmd::Update { name } => {
            let moved = plugins::update(paths, name.as_deref())?;
            if moved.is_empty() {
                say!("already up to date.");
            } else {
                for (n, range) in moved {
                    say!("{n}  {range}");
                }
                say!("\nThe running workspace has the old copy. `neosh stop` to pick these up.");
            }
        }
        PluginCmd::Remove { name, yes } => {
            // Irreversible, so it asks — and says what is at stake before it does. See ADR 0039.
            if !yes {
                let dir = paths.installed_plugin_dir().join(name);
                if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    anyhow::bail!(
                        "removing {name} deletes {} and cannot be undone.\n\
                         Nobody is here to ask, so pass --yes if you meant it.",
                        dir.display()
                    );
                }
                eprint!("Delete {} and everything in it? [y/N] ", dir.display());
                use std::io::Write;
                let _ = std::io::stderr().flush();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    say!("left alone.");
                    return Ok(());
                }
            }
            let dir = plugins::remove(paths, name)?;
            say!("removed {name} ({})", dir.display());
        }
    }
    Ok(())
}

/// The word a permission is spelled with in `plugin.toml`, so a refusal and a listing agree.
fn permission_name(p: neosh_proto::PluginPermission) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{p:?}"))
}

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

    // Paced only when asked. A test that needs a turn to still be running while the terminal is
    // closed under it has no other way to arrange one without a real model and a real wait.
    let mock = match std::env::var("NEOSH_MOCK_DELAY_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(ms) if ms > 0 => MockProvider::new(turns).paced(std::time::Duration::from_millis(ms)),
        _ => MockProvider::new(turns),
    };
    let mut reg = agent.providers_mut();
    reg.register_driver(Arc::new(mock));
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
    let instances: Vec<_> = agent.providers().instances().cloned().collect();
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

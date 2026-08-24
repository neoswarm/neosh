//! `neosh init` — write a starting configuration.
//!
//! A config system whose first step is "create an empty file and read the manual" loses most of its
//! users there. This writes a working, commented `init.ts`, a `config.toml`, and — the part that
//! makes TypeScript worth choosing — a `tsconfig.json` plus the `@neosh/api` types **emitted from
//! this binary**, so the editor autocompletes the API and flags mistakes before neosh ever runs it.
//!
//! Hand-written files are never overwritten without `--force`. Generated ones (`tsconfig.json`,
//! `types/`) always are, because they are outputs, not documents.

use std::path::{Path, PathBuf};

use crate::paths::Paths;

/// What `neosh init` did, so the caller can report it precisely rather than saying "done".
#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub written: Vec<PathBuf>,
    pub kept: Vec<PathBuf>,
}

impl Report {
    fn write(&mut self, path: PathBuf, contents: &str, force: bool) -> anyhow::Result<()> {
        if path.exists() && !force {
            self.kept.push(path);
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        self.written.push(path);
        Ok(())
    }
}

/// Scaffold the user's config directory.
pub fn init_user(paths: &Paths, force: bool) -> anyhow::Result<Report> {
    let mut r = Report::default();
    std::fs::create_dir_all(&paths.config)?;
    std::fs::create_dir_all(paths.plugin_dir())?;

    r.write(paths.init_script(), INIT_TS, force)?;
    r.write(paths.config_file(), CONFIG_TOML, force)?;
    // Generated: always current, never merged. The installed-plugin directory is absolute, so
    // `plugin:<name>` type-checks against a plugin `neosh plugin add` put there as well as one in
    // the config directory and one bundled in the binary.
    let installed = paths.installed_plugin_dir();
    let tsconfig = TSCONFIG.replace("__INSTALLED__", &json_path(&installed));
    r.write(paths.tsconfig(), &tsconfig, true)?;

    let types = paths.api_types();
    neosh_script::write_api_types(&types)
        .map_err(|e| anyhow::anyhow!("{}: {e}", types.display()))?;
    // The bundled plugins' source beside the API's, so `import { api } from "plugin:sidebar"`
    // resolves for `tsc` to the very code the binary runs — the same reason the API types come
    // from the binary rather than a package.
    neosh_script::write_builtin_sources(&types.join("builtin"))
        .map_err(|e| anyhow::anyhow!("{}: {e}", types.display()))?;
    r.written.push(types);

    Ok(r)
}

/// Scaffold `.neosh/` in a workspace.
pub fn init_project(cwd: &Path, force: bool) -> anyhow::Result<Report> {
    let mut r = Report::default();
    std::fs::create_dir_all(Paths::project_dir(cwd))?;
    r.write(Paths::project_config(cwd), PROJECT_TOML, force)?;
    Ok(r)
}

const INIT_TS: &str = r#"/**
 * neosh configuration.
 *
 * This file is a plugin. It is loaded first, before anything else, and it gets the entire public
 * API — the same one every plugin uses, with nothing held back. Anything a plugin can do, your
 * config can do here.
 *
 * Type checking: `npx tsc --noEmit` in this directory. The types next door in `types/` were
 * written by the neosh binary you are running, so they always match it.
 *
 * Reload: press <C-r>. No restart, and settings you delete here go back to their
 * defaults rather than lingering.
 */
import type { PluginContext } from "@neosh/api";
import { picker } from "@neosh/api/ui";

export async function activate({ neosh }: PluginContext) {
  // ── settings ──────────────────────────────────────────────────────────
  // Every option is typed and declared; a typo is an error, not a no-op.
  // `neosh.opt.all()` lists everything available, including options plugins added.

  // Which model to start with, as `instance/model`. Leave unset to pick whatever
  // works without credentials. `neosh --list-models` prints every option.
  // await neosh.opt.set("agent.model", "claude-cli/sonnet");

  // Show the model's reasoning in the chat buffer as it streams.
  await neosh.opt.set("chat.show_thinking", false);

  // await neosh.opt.set("agent.system_prompt", "Answer in British English.");

  // ── plugins ───────────────────────────────────────────────────────────
  // Anything under `plugins/` next to this file loads automatically. Add more
  // directories here — this runs before discovery, which is the point.
  // await neosh.rtp.add("~/src/my-neosh-plugins");

  // ── commands ──────────────────────────────────────────────────────────
  // Commands are named, so they are listable, remappable, and callable by other
  // plugins. Keys bind to command *names*, never to closures.
  // `@neosh/api/ui` has the widgets — a filterable picker, a text prompt, a
  // confirm. All built on the public API, so nothing here is out of your reach.
  // (The bundled `model` plugin already binds <C-p> to a better version of this;
  // it is here because it is the shortest thing that shows the shape.)
  await neosh.cmd.register(
    "models.recent",
    async () => {
      const entries = await neosh.agent.listModels();
      const current = await neosh.agent.selection();
      const chosen = await picker(
        neosh,
        // Each entry pairs the model with the instance serving it: the same model
        // id can come from two different endpoints.
        entries.map((e) => ({
          label: e.model.display_name,
          detail: e.instance,
          keywords: e.model.id,
          value: e,
        })),
        { title: "Models", width: 68 },
      );
      if (!chosen) return;
      await neosh.agent.setSelection({
        instance: chosen.instance,
        model: chosen.model.id,
        options: current?.options ?? [],
      });
    },
    { desc: "Pick a model" },
  );

  // ── keymaps ───────────────────────────────────────────────────────────
  // Neovim notation: <C-x>, <Esc>, <CR>, <F5>, gd, and <leader>.
  // neosh's own commands are ordinary registry entries — `<C-r>` (config.reload)
  // and `<C-q>` (quit) are just defaults, and binding over them replaces them.
  //
  // Avoid <C-m>, <C-i>, <C-h> and <C-j>: a terminal delivers those as Enter, Tab,
  // Backspace and newline, so binding them binds those instead.
  await neosh.keymap.set("chat", "<C-y>", "models.recent", { desc: "Pick a model" });

  // <leader> is whatever `mapleader` says, substituted when the binding is made —
  // so set it first, as in Neovim. Nothing ships bound to it, which is why the
  // leader key stays typeable until you decide otherwise.
  //
  // A sequence you start and do not finish is typed literally after `timeoutlen`
  // milliseconds, so binding `,pa` does not cost you the ability to type a comma.
  await neosh.opt.set("mapleader", ",");
  await neosh.keymap.set("chat", "<leader>pa", "project.open", { desc: "Add a project" });
  await neosh.keymap.set("chat", "<leader>pp", "sidebar.focus", { desc: "Projects" });

  // ── timers ────────────────────────────────────────────────────────────
  // `neosh.timer.*` is cancelled when this config reloads; the global setTimeout
  // is not, because it has no idea who called it.
  //
  // const redraw = neosh.timer.debounce(150, () => { /* ... */ });
  // ctx.subscriptions.push(neosh.agent.onToken(redraw));

  // ── hooks ─────────────────────────────────────────────────────────────
  // An observer sees everything and can change nothing — which is what stops a
  // logging hook from wedging the agent loop.
  await neosh.hook.register("tool_pre", (p) => {
    if (p.hook === "tool_pre") neosh.log.info(`tool: ${p.call.name}`);
    return { action: "continue" };
  });

  // A blocking hook may veto. It is awaited with a timeout, and **a timeout counts
  // as a veto**, so a policy that hangs fails closed rather than open.
  //
  // await neosh.hook.register(
  //   "tool_pre",
  //   (p) =>
  //     p.hook === "tool_pre" && p.call.name === "write_file"
  //       ? { action: "veto", reason: "not in this project" }
  //       : { action: "continue" },
  //   { blocking: true, timeoutMs: 2000 },
  // );

  // ── tools ─────────────────────────────────────────────────────────────
  // A tool you register is indistinguishable from a built-in one, to the model
  // and to the permission layer.
  await neosh.tool.register(
    {
      name: "count_lines",
      description: "Count the lines in a string.",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
    },
    (input) => {
      const text = (input as { text?: string }).text ?? "";
      return { content: String(text.split("\n").length), is_error: false };
    },
  );

  neosh.log.info("config loaded");
}
"#;

const CONFIG_TOML: &str = r#"# neosh — declarative configuration.
#
# This file holds data: values, names, paths. Anything conditional or interactive belongs in
# `init.ts` next door, which gets the full API.
#
# Unknown keys are an error rather than being ignored, so a typo here fails loudly.

[agent]
# Startup model, as `instance/model`. `neosh --list-models` prints every option.
# Leave it out to use whatever works without credentials.
# model = "claude-cli/sonnet"
# system_prompt = "..."

[permissions]
# ask | allow_listed | deny   (`allow_listed` skips prompts for the lists below)
mode = "ask"
allow_commands = []
allow_hosts = []
allow_roots = []

# Values for declared options. Options a plugin declares are settable here too — they apply as soon
# as that plugin loads. Names and types are checked; `neosh.opt.all()` lists what exists.
[options]
# "chat.show_thinking" = true

# Extra provider instances, merged over the built-in catalog by id.
#
# `auth` says *where a key is*, never what it is — nothing writes a secret to this file. To enter
# one instead, run `provider.auth` (or just pick a model that needs one): it goes to your keychain.
# [[providers]]
# id = "my-endpoint"
# driver = "openai-compat"
# display_name = "Local llama.cpp"
# base_url = "http://localhost:8080/v1"
# auth = { kind = "env", var = "MY_API_KEY" }
# auth = { kind = "command", argv = ["pass", "show", "my-endpoint/key"] }  # or a helper
# auth = { kind = "none" }                                                # or nothing at all

# Extra plugin directories. Relative paths are relative to this file.
# plugin_dirs = ["~/src/neosh-plugins"]
"#;

const PROJECT_TOML: &str = r#"# neosh — project configuration.
#
# Commit this. It applies to anyone who opens this repository in neosh.
#
# ## What applies immediately
#
#   model         — can only name an instance the user already configured
#   [permissions] — may only make things *stricter*, never looser
#
# ## What waits for `neosh trust`
#
#   system_prompt, plugin_dirs, providers, [options], and `.neosh/init.ts`
#
# Those can run code or widen what the agent may do, so they are held until someone reads the file
# and runs `neosh trust`. Trust is keyed on the file contents: editing this file revokes it, which
# means a `git pull` cannot quietly change what runs on your machine.

# model = "anthropic/claude-opus-5"

[permissions]
# mode = "allow_listed"
# allow_commands = ["cargo"]   # narrowed against your global list; cannot add to it
"#;

const TSCONFIG: &str = r#"{
  "//": "Generated by `neosh init`. Rewritten on every run; put your own settings elsewhere.",
  "compilerOptions": {
    "target": "es2022",
    "module": "esnext",
    "moduleResolution": "bundler",
    "lib": ["es2022"],
    "types": [],
    "strict": true,
    "noEmit": true,
    "skipLibCheck": true,
    "baseUrl": ".",
    "paths": {
      "@neosh/api": ["./types/index.ts"],
      "@neosh/api/*": ["./types/*.ts"],
      "plugin:*": ["./plugins/*/main.ts", __INSTALLED__, "./types/builtin/*/main.ts"]
    }
  },
  "include": ["init.ts", "plugins/**/*.ts", "types/globals.d.ts"]
}
"#;

/// A path as a JSON string literal, with a `/*/main.ts` glob on the end for `paths`.
fn json_path(dir: &Path) -> String {
    let joined = dir.join("*").join("main.ts");
    serde_json::to_string(&joined.to_string_lossy()).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "neosh-scaffold-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        Tmp(p)
    }

    fn paths(t: &Tmp) -> Paths {
        Paths {
            config: t.0.join("config"),
            data: t.0.join("data"),
            state: t.0.join("state"),
            cache: t.0.join("cache"),
            clean: false,
        }
    }

    #[test]
    fn init_writes_a_config_that_is_ready_to_edit() {
        let t = tmp("user");
        let p = paths(&t);
        init_user(&p, false).unwrap();
        assert!(p.init_script().is_file());
        assert!(p.config_file().is_file());
        assert!(p.tsconfig().is_file());
        assert!(p.plugin_dir().is_dir(), "somewhere to drop a plugin");
        // The types are what makes TypeScript worth choosing; without them `init.ts` is untyped.
        assert!(p.api_types().join("index.ts").is_file());
        assert!(p.api_types().join("generated").is_dir());
    }

    #[test]
    fn the_emitted_types_are_this_binarys_types() {
        let t = tmp("types");
        let p = paths(&t);
        init_user(&p, false).unwrap();
        assert!(neosh_script::api_types_are_current(&p.api_types()));
        assert_eq!(
            std::fs::read_to_string(p.api_types().join("index.ts")).unwrap(),
            neosh_script::API_SOURCE,
            "the types must be the same file the host executes"
        );
    }

    #[test]
    fn tsconfig_resolves_the_api_submodules_too() {
        // `@neosh/api/ui` is a real import in the starter config. Without the wildcard mapping the
        // first thing a new user sees is a red squiggle on line two of a file they did not write.
        assert!(TSCONFIG.contains(r#""@neosh/api/*": ["./types/*.ts"]"#));
    }

    #[test]
    fn tsconfig_points_at_the_emitted_types() {
        let t = tmp("tsconfig");
        let p = paths(&t);
        init_user(&p, false).unwrap();
        let text = std::fs::read_to_string(p.tsconfig()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(json["compilerOptions"]["paths"]["@neosh/api"][0], "./types/index.ts");
    }

    #[test]
    fn a_second_init_does_not_clobber_your_config() {
        let t = tmp("keep");
        let p = paths(&t);
        init_user(&p, false).unwrap();
        std::fs::write(p.init_script(), "// mine\n").unwrap();

        let r = init_user(&p, false).unwrap();
        assert_eq!(std::fs::read_to_string(p.init_script()).unwrap(), "// mine\n");
        assert!(r.kept.contains(&p.init_script()));
        // Generated files are refreshed regardless: they are output, not a document.
        assert!(r.written.contains(&p.tsconfig()));
    }

    #[test]
    fn force_overwrites_hand_written_files() {
        let t = tmp("force");
        let p = paths(&t);
        init_user(&p, false).unwrap();
        std::fs::write(p.init_script(), "// mine\n").unwrap();
        init_user(&p, true).unwrap();
        assert_ne!(std::fs::read_to_string(p.init_script()).unwrap(), "// mine\n");
    }

    #[test]
    fn the_scaffolded_config_toml_actually_parses() {
        // A starter config that does not load is worse than no starter config.
        let t = tmp("parses");
        let p = paths(&t);
        init_user(&p, false).unwrap();
        crate::config::Config::load_from(&p.config_file()).expect("scaffold must be valid");
    }

    #[test]
    fn the_scaffolded_project_config_actually_parses() {
        let t = tmp("project");
        std::fs::create_dir_all(&t.0).unwrap();
        init_project(&t.0, false).unwrap();
        let text = std::fs::read_to_string(Paths::project_config(&t.0)).unwrap();
        toml::from_str::<crate::config::ProjectConfig>(&text).expect("scaffold must be valid");
    }

    #[test]
    fn the_scaffolded_init_only_uses_api_that_exists() {
        // Cheap guard against the template drifting away from the API it demonstrates. The real
        // check is `tsc --noEmit` in scripts/check.sh; this catches a rename at `cargo test` speed.
        for name in [
            "opt.set", "rtp.add", "cmd.register", "keymap.set", "hook.register", "tool.register",
            "agent.listModels", "agent.setSelection", "timer.debounce", "agent.onToken",
        ] {
            let method = name.split('.').nth(1).unwrap();
            assert!(INIT_TS.contains(name), "template no longer demonstrates {name}");
            assert!(
                neosh_script::API_SOURCE.contains(method),
                "the API no longer has {name}; the starter config would fail to type-check"
            );
        }
        // The template imports the widget library, so the submodule has to exist and export it —
        // otherwise every new user's first `tsc` fails on line two.
        assert!(INIT_TS.contains(r#"from "@neosh/api/ui""#));
        let ui = neosh_script::API_TREE
            .get_file("ui.ts")
            .and_then(|f| f.contents_utf8())
            .expect("@neosh/api/ui is embedded");
        assert!(ui.contains("export async function picker"), "ui no longer exports picker");
    }
}

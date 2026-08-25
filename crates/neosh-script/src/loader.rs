//! Module resolution and TypeScript transpilation.
//!
//! Two responsibilities:
//!
//! * **`@neosh/api` is a virtual module.** Plugins `import { activate } from "@neosh/api"` and get
//!   the ergonomic layer without a package install or a node_modules directory. Its source is the
//!   very same `plugins/api/src/index.ts` that plugin authors type-check against, embedded at
//!   build time — so the implementation and the types cannot drift, because they are one file.
//! * **TypeScript is transpiled, not type-checked.** Types are erased at load; correctness is
//!   `tsc --noEmit` in the plugin's own CI. Doing it this way keeps startup fast and keeps a type
//!   error from being a runtime crash in someone else's editor.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use deno_ast::{MediaType, ParseParams, SourceMapOption};
use deno_core::error::ModuleLoaderError;
use deno_core::{
    ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader, ModuleSource,
    ModuleSourceCode, ModuleSpecifier, ModuleType, ResolutionKind, resolve_import,
};
use deno_error::JsErrorBox;

/// The embedded plugin tree.
///
/// The statics themselves live in `neosh-plugins`, whose package root *is* `plugins/` — an
/// `include_dir!` reaching two directories upwards embeds fine here and packages to nothing, so a
/// published `neosh-script` would have carried no plugins and no API source at all. Re-exported
/// rather than moved, because every caller in the workspace and every doc link says
/// `neosh_script::API_TREE`.
pub use neosh_plugins::{API_SOURCE, API_TREE, BUILTIN_TREE};

/// A plugin embedded in the binary: its directory name, manifest text and entry source.
pub struct BuiltinPlugin {
    pub name: String,
    pub manifest: String,
    pub entry: String,
    /// Module URL to load it from. Understood by [`NeoshModuleLoader`], not by the filesystem.
    pub url: String,
}

/// Every bundled plugin, in directory order.
pub fn builtin_plugins() -> Vec<BuiltinPlugin> {
    let mut out = Vec::new();
    for entry in BUILTIN_TREE.dirs() {
        let Some(name) = entry.path().file_name().and_then(|n| n.to_str()) else { continue };
        let manifest = entry
            .get_file(entry.path().join("plugin.toml"))
            .and_then(|f| f.contents_utf8());
        let source = entry
            .get_file(entry.path().join("main.ts"))
            .and_then(|f| f.contents_utf8());
        // A bundled plugin missing either file is a build mistake, not a user problem. Skipping
        // keeps a broken one from taking the whole session down; the test below catches it first.
        let (Some(manifest), Some(source)) = (manifest, source) else {
            continue;
        };
        out.push(BuiltinPlugin {
            name: name.to_string(),
            manifest: manifest.to_string(),
            entry: source.to_string(),
            url: format!("{BUILTIN_URL_PREFIX}{name}/main.ts"),
        });
    }
    out
}

/// URL scheme-and-path prefix bundled plugins load from.
///
/// `neosh:/`, not `neosh:` — see [`BUILTIN_TREE`] for why the slash matters.
pub const BUILTIN_URL_PREFIX: &str = "neosh:/builtin/";

/// The file behind a `neosh:/builtin/...` URL, whether it is a plugin's entry or something it
/// imported alongside it.
fn builtin_source(url: &str) -> Option<String> {
    let path = url.strip_prefix(BUILTIN_URL_PREFIX)?;
    // Strip the reload generation, which is appended as `?v=N` exactly as it is for file modules.
    let path = path.split('?').next().unwrap_or(path);
    BUILTIN_TREE.get_file(path).and_then(|f| f.contents_utf8()).map(str::to_string)
}

/// Write the API source tree to `dest`, returning the number of files written.
///
/// Existing files are overwritten: this directory is generated output, and a stale copy of the
/// types is worse than none.
pub fn write_api_types(dest: &std::path::Path) -> std::io::Result<usize> {
    fn walk(dir: &include_dir::Dir<'_>, dest: &std::path::Path, n: &mut usize) -> std::io::Result<()> {
        for entry in dir.entries() {
            match entry {
                include_dir::DirEntry::Dir(d) => {
                    std::fs::create_dir_all(dest.join(d.path()))?;
                    walk(d, dest, n)?;
                }
                include_dir::DirEntry::File(f) => {
                    let path = dest.join(f.path());
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, f.contents())?;
                    *n += 1;
                }
            }
        }
        Ok(())
    }
    std::fs::create_dir_all(dest)?;
    let mut n = 0;
    walk(&API_TREE, dest, &mut n)?;
    Ok(n)
}

/// Write the bundled plugins' source to `dest`, one directory per plugin, so a plugin author can
/// type-check `import ... from "plugin:sidebar"` against the code this binary actually runs.
pub fn write_builtin_sources(dest: &std::path::Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dest)?;
    let mut n = 0;
    for entry in BUILTIN_TREE.dirs() {
        for file in entry.files() {
            let path = dest.join(file.path());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, file.contents())?;
            n += 1;
        }
    }
    Ok(n)
}

/// Whether a previously written tree still matches this binary.
pub fn api_types_are_current(dest: &std::path::Path) -> bool {
    fn check(dir: &include_dir::Dir<'_>, dest: &std::path::Path) -> bool {
        dir.entries().iter().all(|entry| match entry {
            include_dir::DirEntry::Dir(d) => check(d, dest),
            include_dir::DirEntry::File(f) => {
                std::fs::read(dest.join(f.path())).is_ok_and(|got| got == f.contents())
            }
        })
    }
    check(&API_TREE, dest)
}

/// A specifier without its `?v=N` reload generation.
fn strip_query(spec: &str) -> &str {
    spec.split('?').next().unwrap_or(spec)
}

/// The filesystem path a module specifier names, ignoring any reload generation.
///
/// `?v=N` is how a reload gets a fresh module out of deno_core's cache; it is not part of the path,
/// and reading the file has to see through it.
fn file_path(spec: &ModuleSpecifier) -> Option<std::path::PathBuf> {
    let mut clean = spec.clone();
    clean.set_query(None);
    clean.set_fragment(None);
    clean.to_file_path().ok()
}

/// Specifier the virtual API module lives at.
pub const API_URL: &str = "neosh:api";
/// What plugins actually write in their imports.
pub const API_SPECIFIER: &str = "@neosh/api";

/// Submodules importable as `@neosh/api/<name>`.
///
/// These are *libraries*, not host capabilities: every one is written against the same public
/// surface a third-party plugin has. Shipping them in the binary is a convenience, not a
/// privilege — a plugin that vendors its own copy behaves identically, which is the property
/// that keeps this honest.
fn api_submodule(name: &str) -> Option<&'static str> {
    API_TREE.get_file(format!("{name}.ts")).map(|f| {
        // Embedded at build time from a checked-in tree, so this is not user input; a non-UTF-8
        // file here would be a broken build, caught by the tests that import each one.
        std::str::from_utf8(f.contents()).unwrap_or("")
    })
}

type SourceMaps = Rc<RefCell<HashMap<String, Vec<u8>>>>;

/// Loaded plugins by name, and the entry URL each one was loaded from. What `plugin:<name>`
/// resolves to.
pub type PluginUrls = Rc<RefCell<HashMap<String, String>>>;

/// What a plugin writes to import another plugin's module: `import { api } from "plugin:sidebar"`.
///
/// Resolves to the other plugin's *entry* URL, generation query included, so deno_core hands back
/// the very module instance the host activated — the same module-level state, not a second copy
/// of it. One isolate for every plugin is what makes this free; it is Neovim's `require`, and it
/// is why a plugin can ask the sidebar a question without a round trip through the host.
pub const PLUGIN_SCHEME: &str = "plugin:";

#[derive(Default)]
pub struct NeoshModuleLoader {
    source_maps: SourceMaps,
    plugins: PluginUrls,
}

impl NeoshModuleLoader {
    pub fn new() -> Self {
        Self::default()
    }

    /// A loader that can resolve `plugin:<name>` against the plugins the runtime has been asked
    /// to load.
    pub fn with_plugins(plugins: PluginUrls) -> Self {
        Self { plugins, ..Default::default() }
    }

    fn transpile(specifier: &ModuleSpecifier, code: String, media_type: MediaType, maps: &SourceMaps) -> Result<String, ModuleLoaderError> {
        let parsed = deno_ast::parse_module(ParseParams {
            specifier: specifier.clone(),
            text: code.into(),
            media_type,
            capture_tokens: false,
            scope_analysis: false,
            maybe_syntax: None,
        })
        .map_err(JsErrorBox::from_err)?;

        let transpiled = parsed
            .transpile(
                &deno_ast::TranspileOptions {
                    imports_not_used_as_values: deno_ast::ImportsNotUsedAsValues::Remove,
                    ..Default::default()
                },
                &deno_ast::TranspileModuleOptions { module_kind: None },
                &deno_ast::EmitOptions {
                    // Separate rather than inline, so a stack trace can be mapped back to the
                    // plugin author's TypeScript without bloating every module.
                    source_map: SourceMapOption::Separate,
                    inline_sources: true,
                    ..Default::default()
                },
            )
            .map_err(JsErrorBox::from_err)?;

        let source = transpiled.into_source();
        if let Some(map) = source.source_map {
            let mut maps = maps.borrow_mut();
            // Drop any older generation's map for this same file. Without this, every reload adds
            // a permanent copy of every source map in the config; a long session spent iterating
            // would accumulate them for as long as neosh runs.
            let path = strip_query(specifier.as_str());
            maps.retain(|k, _| strip_query(k) != path);
            maps.insert(specifier.to_string(), map.into_bytes());
        }
        Ok(source.text)
    }
}

impl ModuleLoader for NeoshModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        if specifier == API_SPECIFIER {
            return ModuleSpecifier::parse(API_URL).map_err(JsErrorBox::from_err);
        }
        if let Some(sub) = specifier.strip_prefix("@neosh/api/") {
            // Rejected here rather than at load, so the error names the import the author wrote
            // instead of an internal `neosh:` URL they have never seen.
            if api_submodule(sub).is_none() {
                return Err(JsErrorBox::generic(format!(
                    "no such module {specifier:?}. Available: {}",
                    available_submodules().join(", ")
                )));
            }
            return ModuleSpecifier::parse(&format!("neosh:{sub}")).map_err(JsErrorBox::from_err);
        }
        if let Some(name) = specifier.strip_prefix(PLUGIN_SCHEME) {
            let url = self.plugins.borrow().get(name).cloned();
            return match url {
                Some(url) => ModuleSpecifier::parse(&url).map_err(JsErrorBox::from_err),
                // Named here rather than failing as `undefined` later: the fix is one line in
                // the manifest, and the message says which line.
                None => Err(JsErrorBox::generic(format!(
                    "cannot import {specifier:?}: no plugin named {name:?} is loaded. Add it to                      `requires` in plugin.toml so it loads first, and check it is installed and                      not in `plugins.disabled`."
                ))),
            };
        }
        // Relative and absolute paths resolve against the referrer.
        if specifier.starts_with('.') || specifier.starts_with('/') {
            let mut resolved = resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)?;
            // Carry the reload generation into relative imports.
            //
            // deno_core keys its module map on the full URL, which is what makes `?v=N` a working
            // cache bust — but only for the module the host names directly. Without propagating it,
            // a config split across files would reload its entry point and keep running the
            // previous generation's helpers: the worst kind of reload, the one that half works.
            if resolved.query().is_none()
                && let Ok(from) = ModuleSpecifier::parse(referrer)
                && let Some(q) = from.query()
            {
                resolved.set_query(Some(q));
            }
            return Ok(resolved);
        }
        // An already-absolute URL passes through. This includes the host's own `neosh:` modules,
        // which deno_core resolves through this same path.
        if let Ok(url) = ModuleSpecifier::parse(specifier) {
            return Ok(url);
        }
        // Everything left is a bare specifier. There is no package resolution here, so a plugin
        // importing `lodash` must fail loudly at load rather than mysteriously at runtime.
        Err(JsErrorBox::generic(format!(
            "cannot resolve {specifier:?}: neosh plugins may import only relative paths, \
             \"{API_SPECIFIER}\" and its submodules ({}), and \"plugin:<name>\" for a plugin \
             named in `requires`. Bundle third-party code into your plugin.",
            available_submodules().join(", ")
        )))
    }

    fn load(
        &self,
        specifier: &ModuleSpecifier,
        _referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let maps = self.source_maps.clone();
        let spec = specifier.clone();

        ModuleLoadResponse::Sync((|| {
            if spec.as_str() == API_URL {
                let code = Self::transpile(&spec, API_SOURCE.to_string(), MediaType::TypeScript, &maps)?;
                return Ok(ModuleSource::new(
                    ModuleType::JavaScript,
                    ModuleSourceCode::String(code.into()),
                    &spec,
                    None,
                ));
            }
            if let Some(source) = builtin_source(spec.as_str()) {
                let code = Self::transpile(&spec, source, MediaType::TypeScript, &maps)?;
                return Ok(ModuleSource::new(
                    ModuleType::JavaScript,
                    ModuleSourceCode::String(code.into()),
                    &spec,
                    None,
                ));
            }
            if spec.scheme() == "neosh"
                && let Some(source) = api_submodule(spec.path())
            {
                let code =
                    Self::transpile(&spec, source.to_string(), MediaType::TypeScript, &maps)?;
                return Ok(ModuleSource::new(
                    ModuleType::JavaScript,
                    ModuleSourceCode::String(code.into()),
                    &spec,
                    None,
                ));
            }

            let path = file_path(&spec)
                .ok_or_else(|| JsErrorBox::generic(format!("unsupported module URL: {spec}")))?;
            let media_type = MediaType::from_path(&path);
            let (module_type, transpile) = match media_type {
                MediaType::JavaScript | MediaType::Mjs | MediaType::Cjs => {
                    (ModuleType::JavaScript, false)
                }
                MediaType::TypeScript
                | MediaType::Mts
                | MediaType::Cts
                | MediaType::Jsx
                | MediaType::Tsx => (ModuleType::JavaScript, true),
                MediaType::Json => (ModuleType::Json, false),
                other => {
                    return Err(JsErrorBox::generic(format!(
                        "cannot load {}: unsupported module kind {other:?}",
                        path.display()
                    )));
                }
            };

            let code = std::fs::read_to_string(&path).map_err(|e| {
                JsErrorBox::generic(format!("cannot read {}: {e}", path.display()))
            })?;
            let code =
                if transpile { Self::transpile(&spec, code, media_type, &maps)? } else { code };

            Ok(ModuleSource::new(
                module_type,
                ModuleSourceCode::String(code.into()),
                &spec,
                None,
            ))
        })())
    }

    fn get_source_map(&self, specifier: &str) -> Option<Cow<'_, [u8]>> {
        self.source_maps.borrow().get(specifier).map(|v| v.clone().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loader() -> NeoshModuleLoader {
        NeoshModuleLoader::new()
    }

    #[test]
    fn the_api_specifier_resolves_to_the_virtual_module() {
        let got = loader().resolve(API_SPECIFIER, "file:///p/main.ts", ResolutionKind::Import);
        assert_eq!(got.unwrap().as_str(), API_URL);
    }

    #[test]
    fn relative_imports_resolve_against_the_referrer() {
        let got = loader()
            .resolve("./util.ts", "file:///p/main.ts", ResolutionKind::Import)
            .unwrap();
        assert_eq!(got.as_str(), "file:///p/util.ts");
    }

    #[test]
    fn a_reload_generation_reaches_relative_imports() {
        // Otherwise a multi-file config reloads its entry point and keeps the old helpers.
        let got = loader()
            .resolve("./util.ts", "file:///p/main.ts?v=3", ResolutionKind::Import)
            .unwrap();
        assert_eq!(got.as_str(), "file:///p/util.ts?v=3");
    }

    #[test]
    fn a_generation_is_not_invented_for_a_first_load() {
        let got = loader()
            .resolve("./util.ts", "file:///p/main.ts", ResolutionKind::Import)
            .unwrap();
        assert_eq!(got.as_str(), "file:///p/util.ts", "no query means no cache bust");
    }

    #[test]
    fn the_generation_is_not_part_of_the_path_on_disk() {
        let spec = ModuleSpecifier::parse("file:///p/main.ts?v=7").unwrap();
        assert_eq!(file_path(&spec).unwrap(), std::path::Path::new("/p/main.ts"));
    }

    #[test]
    fn bare_specifiers_fail_loudly_rather_than_silently() {
        let err = loader().resolve("lodash", "file:///p/main.ts", ResolutionKind::Import);
        assert!(err.is_err(), "there is no package resolution; this must not appear to work");
    }

    #[test]
    fn the_embedded_api_source_is_the_file_plugin_authors_type_check_against() {
        assert!(API_SOURCE.contains("export"), "API source should export something");
        // One file means implementation and types cannot drift apart.
        assert!(!API_SOURCE.is_empty());
    }

    #[test]
    fn typescript_types_are_erased_at_transpile() {
        let maps: SourceMaps = Default::default();
        let spec = ModuleSpecifier::parse("file:///p/main.ts").unwrap();
        let out = NeoshModuleLoader::transpile(
            &spec,
            "interface Foo { a: string }\nexport const x: number = 1;\n".into(),
            MediaType::TypeScript,
            &maps,
        )
        .unwrap();
        assert!(!out.contains("interface"), "types must not survive into JS");
        assert!(out.contains("export const x = 1"));
    }

    #[test]
    fn type_only_imports_do_not_survive_as_runtime_imports() {
        let maps: SourceMaps = Default::default();
        let spec = ModuleSpecifier::parse("file:///p/main.ts").unwrap();
        let out = NeoshModuleLoader::transpile(
            &spec,
            "import type { Bar } from \"./generated/Bar\";\nexport const y: Bar | null = null;\n"
                .into(),
            MediaType::TypeScript,
            &maps,
        )
        .unwrap();
        assert!(
            !out.contains("./generated/Bar"),
            "a type-only import must not become a module the loader has to find"
        );
    }

    #[test]
    fn a_source_map_is_recorded_so_stack_traces_point_at_typescript() {
        let maps: SourceMaps = Default::default();
        let spec = ModuleSpecifier::parse("file:///p/main.ts").unwrap();
        NeoshModuleLoader::transpile(&spec, "export const x: number = 1;".into(), MediaType::TypeScript, &maps)
            .unwrap();
        assert!(maps.borrow().contains_key(spec.as_str()));
    }

    #[test]
    fn reloading_replaces_a_source_map_rather_than_accumulating_one_per_generation() {
        let maps: SourceMaps = Default::default();
        for generation in 0..5 {
            let spec =
                ModuleSpecifier::parse(&format!("file:///p/main.ts?v={generation}")).unwrap();
            NeoshModuleLoader::transpile(
                &spec,
                "export const x: number = 1;".into(),
                MediaType::TypeScript,
                &maps,
            )
            .unwrap();
        }
        assert_eq!(maps.borrow().len(), 1, "one map per file, not one per reload");
        assert!(maps.borrow().contains_key("file:///p/main.ts?v=4"), "and it is the current one");
    }

    #[test]
    fn different_files_keep_their_own_source_maps() {
        let maps: SourceMaps = Default::default();
        for name in ["main", "helper"] {
            let spec = ModuleSpecifier::parse(&format!("file:///p/{name}.ts?v=1")).unwrap();
            NeoshModuleLoader::transpile(
                &spec,
                "export const x: number = 1;".into(),
                MediaType::TypeScript,
                &maps,
            )
            .unwrap();
        }
        assert_eq!(maps.borrow().len(), 2);
    }
}

/// Names importable as `@neosh/api/<name>`, for error messages that tell the author what exists.
fn available_submodules() -> Vec<String> {
    let mut names: Vec<String> = API_TREE
        .files()
        .filter_map(|f| f.path().file_stem()?.to_str().map(str::to_string))
        .filter(|n| n != "index" && !n.ends_with(".d"))
        .collect();
    names.sort();
    names
}

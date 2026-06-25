//! # Package tooling for Rune
//!
//! Adds a package manifest (`rune.toml`) and package-aware resolution on top of
//! the frozen `rune` library. A *package* is a directory containing a
//! `rune.toml` and an entry source file (default `src/main.rune`). Packages may
//! depend on other local packages by path; a dependency's top-level items are
//! exposed under a module named after the dependency, so a consumer can write
//! `use mathx::clamp;` or `mathx::clamp(...)`.
//!
//! Resolution assembles a single [`rune::ast::Program`] in this order:
//! `std` module (if found) → one `mod <dep>` per dependency → the package's own
//! items. [`build`] then runs that program through `rune::typeck::check`.
//!
//! ## std injection
//!
//! We deliberately use [`rune::load_program`] (not `rune::compile_path`) for the
//! entry file so that `std` is injected exactly once, here, via
//! [`rune::loader::find_std_dir`] + [`rune::loader::load_std`]. Tests point
//! `RUNE_STD` at the workspace `std/`; in normal use the entry-relative or
//! `./std` lookup applies.

use rune::ast;
use rune::diagnostic::{Diagnostic, Stage};
use rune::span::Span;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A parsed `rune.toml` manifest.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// Entry source file, relative to the package directory. Defaults to
    /// `src/main.rune`.
    pub entry: String,
    pub deps: Vec<Dep>,
}

/// A single path dependency: another Rune package on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct Dep {
    pub name: String,
    /// Dependency package directory, relative to the depending package.
    pub path: PathBuf,
}

// ---- raw serde shapes ----------------------------------------------------

#[derive(Deserialize)]
struct RawManifest {
    package: RawPackage,
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, RawDep>,
}

#[derive(Deserialize)]
struct RawPackage {
    name: String,
    version: String,
    entry: Option<String>,
}

#[derive(Deserialize)]
struct RawDep {
    path: String,
}

/// Read and parse `dir/rune.toml`.
pub fn load_manifest(dir: &Path) -> Result<Manifest, String> {
    let manifest_path = dir.join("rune.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read `{}`: {}", manifest_path.display(), e))?;
    let raw: RawManifest = toml::from_str(&text)
        .map_err(|e| format!("invalid `{}`: {}", manifest_path.display(), e))?;

    let mut deps: Vec<Dep> = raw
        .dependencies
        .into_iter()
        .map(|(name, d)| Dep {
            name,
            path: PathBuf::from(d.path),
        })
        .collect();
    // Deterministic order (BTreeMap already sorts keys, but be explicit).
    deps.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Manifest {
        name: raw.package.name,
        version: raw.package.version,
        entry: raw.package.entry.unwrap_or_else(|| "src/main.rune".to_string()),
        deps,
    })
}

/// The absolute (canonicalized) entry file for a package directory.
fn entry_path(dir: &Path, manifest: &Manifest) -> PathBuf {
    dir.join(&manifest.entry)
}

fn diag(msg: impl Into<String>) -> Vec<Diagnostic> {
    vec![Diagnostic::new(Stage::Parse, msg.into(), Span::dummy())]
}

/// Assemble the full program for the package in `dir`: the package's own items,
/// each dependency wrapped in a `mod <dep>`, and the `std` module (if found),
/// in load order std → deps → package.
pub fn resolve_program(dir: &Path) -> Result<ast::Program, Vec<Diagnostic>> {
    let mut visited = HashSet::new();
    resolve_program_inner(dir, &mut visited, true)
}

/// `inject_std` is true only for the root package; dependency programs do not
/// re-inject `std` (it is shared, injected once at the root).
fn resolve_program_inner(
    dir: &Path,
    visited: &mut HashSet<PathBuf>,
    inject_std: bool,
) -> Result<ast::Program, Vec<Diagnostic>> {
    // Cycle guard: canonicalize the package dir and refuse to revisit it.
    let key = dir
        .canonicalize()
        .map_err(|e| diag(format!("cannot access package `{}`: {}", dir.display(), e)))?;
    if !visited.insert(key.clone()) {
        return Err(diag(format!(
            "dependency cycle detected at `{}`",
            dir.display()
        )));
    }

    let manifest = load_manifest(dir).map_err(diag)?;
    let entry = entry_path(dir, &manifest);

    // The package's own program (its file modules already resolved).
    let mut program = rune::load_program(&entry)?;

    // Wrap each dependency's top-level items in `mod <dep>` and prepend.
    let mut dep_mods: Vec<ast::Item> = Vec::new();
    for dep in &manifest.deps {
        let dep_dir = dir.join(&dep.path);
        let dep_program = resolve_program_inner(&dep_dir, visited, false)?;
        dep_mods.push(ast::Item::Mod(ast::ModDef {
            name: dep.name.clone(),
            items: Some(dep_program.items),
            span: Span::dummy(),
        }));
    }
    // Prepend dep modules before the package's own items.
    for item in dep_mods.into_iter().rev() {
        program.items.insert(0, item);
    }

    // Inject std exactly once, at the root.
    if inject_std {
        if let Some(std_dir) = rune::loader::find_std_dir(Some(&entry)) {
            let std_item = rune::loader::load_std(&std_dir)?;
            program.items.insert(0, std_item);
        }
    }

    // Allow this directory to be revisited along a different (non-cyclic) path.
    visited.remove(&key);
    Ok(program)
}

/// Resolve and typecheck the package in `dir`, yielding its typed IR module.
pub fn build(dir: &Path) -> Result<rune::ir::Module, Vec<Diagnostic>> {
    let program = resolve_program(dir)?;
    rune::typeck::check(&program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_std() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("std")
    }

    fn with_std<T>(f: impl FnOnce() -> T) -> T {
        // Tests in this module run in-process; setting RUNE_STD here is fine for
        // the unit tests that need std but is kept minimal.
        std::env::set_var("RUNE_STD", workspace_std());
        f()
    }

    #[test]
    fn manifest_parse_minimal() {
        let dir = unique_dir("pkg_manifest");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rune.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
        let m = load_manifest(&dir).unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.entry, "src/main.rune");
        assert!(m.deps.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_parse_with_deps_and_entry() {
        let dir = unique_dir("pkg_manifest_deps");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rune.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nentry = \"src/lib.rune\"\n\n[dependencies]\nmathx = { path = \"../mathx\" }\n",
        )
        .unwrap();
        let m = load_manifest(&dir).unwrap();
        assert_eq!(m.entry, "src/lib.rune");
        assert_eq!(m.deps.len(), 1);
        assert_eq!(m.deps[0].name, "mathx");
        assert_eq!(m.deps[0].path, PathBuf::from("../mathx"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_simple_package() {
        with_std(|| {
            let dir = unique_dir("pkg_build");
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("rune.toml"),
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            )
            .unwrap();
            std::fs::write(
                dir.join("src/main.rune"),
                "fn answer() -> u32 { 42 }\nfn main() { print(answer()); }\n",
            )
            .unwrap();
            let module = build(&dir).expect("builds");
            assert!(module.funcs.contains_key("answer"));
            assert!(module.funcs.contains_key("main"));
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rune_{}_{}_{}",
            tag,
            std::process::id(),
            n
        ))
    }
}

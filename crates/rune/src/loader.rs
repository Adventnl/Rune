//! # Module loader
//!
//! Resolves file-based modules. A declaration `mod name;` (with no inline body)
//! is loaded from a sibling source file `name.rune` in the same directory as the
//! declaring file; that module's own file submodules live in a `name/`
//! subdirectory (Rust-style). Inline modules (`mod name { ... }`) are left
//! untouched but their file submodules are still resolved.
//!
//! The loader stitches everything into a single [`ast::Program`] that the
//! typechecker consumes. Note: because diagnostics currently index a single
//! source string, type/parse errors *inside included files* are reported with
//! the file name in the message but without a source caret. Errors in the entry
//! file render normally.

use crate::ast;
use crate::diagnostic::{Diagnostic, Stage};
use crate::span::Span;
use std::path::{Path, PathBuf};

/// Load an entry file and recursively resolve its file-based modules into one
/// [`ast::Program`].
pub fn load_program(entry: &Path) -> Result<ast::Program, Vec<Diagnostic>> {
    let src = std::fs::read_to_string(entry).map_err(|e| {
        vec![Diagnostic::new(
            Stage::Parse,
            format!("cannot read `{}`: {}", entry.display(), e),
            Span::dummy(),
        )]
    })?;
    let mut program = parse_source(&src, entry)?;
    let base_dir = entry.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let mut diags = Vec::new();
    resolve_mods(&mut program.items, &base_dir, &mut diags);
    if diags.is_empty() {
        Ok(program)
    } else {
        Err(diags)
    }
}

/// Compile an entry file (resolving file modules) straight to a typed
/// [`crate::ir::Module`]. If a standard-library directory is found (see
/// [`find_std_dir`]), its modules are injected under `std::` automatically.
pub fn compile_path(entry: &Path) -> Result<crate::ir::Module, Vec<Diagnostic>> {
    let mut program = load_program(entry)?;
    if let Some(std_dir) = find_std_dir(Some(entry)) {
        let std_item = load_std(&std_dir)?;
        program.items.insert(0, std_item);
    }
    crate::typeck::check(&program)
}

/// Load every `*.rune` file in `std_dir` as a submodule and wrap them in a
/// single top-level `mod std { ... }` item. File `math.rune` becomes
/// `mod math { ... }`, usable as `std::math::...`.
pub fn load_std(std_dir: &Path) -> Result<ast::Item, Vec<Diagnostic>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(std_dir)
        .map_err(|e| {
            vec![Diagnostic::new(
                Stage::Parse,
                format!("cannot read std dir `{}`: {}", std_dir.display(), e),
                Span::dummy(),
            )]
        })?
        .filter_map(|r| r.ok().map(|e| e.path()))
        .filter(|p| p.extension().map_or(false, |x| x == "rune"))
        .collect();
    entries.sort();

    let mut submods: Vec<ast::Item> = Vec::new();
    let mut diags: Vec<Diagnostic> = Vec::new();
    for file in &entries {
        let name = file.file_stem().unwrap().to_string_lossy().to_string();
        match load_module_file(file) {
            Ok(mut items) => {
                let child_dir = std_dir.join(&name);
                resolve_mods(&mut items, &child_dir, &mut diags);
                submods.push(ast::Item::Mod(ast::ModDef {
                    name,
                    items: Some(items),
                    span: Span::dummy(),
                }));
            }
            Err(mut ds) => diags.append(&mut ds),
        }
    }
    if !diags.is_empty() {
        return Err(diags);
    }
    Ok(ast::Item::Mod(ast::ModDef {
        name: "std".to_string(),
        items: Some(submods),
        span: Span::dummy(),
    }))
}

fn parse_source(src: &str, path: &Path) -> Result<ast::Program, Vec<Diagnostic>> {
    let tokens = crate::lexer::lex(src).map_err(|d| vec![relabel(d, path)])?;
    crate::parser::parse(&tokens).map_err(|d| vec![relabel(d, path)])
}

/// Prefix a diagnostic's message with the file it came from (included files have
/// no shared source for caret rendering).
fn relabel(mut d: Diagnostic, path: &Path) -> Diagnostic {
    d.message = format!("{} (in {})", d.message, path.display());
    d
}

fn resolve_mods(items: &mut [ast::Item], dir: &Path, diags: &mut Vec<Diagnostic>) {
    for item in items.iter_mut() {
        if let ast::Item::Mod(m) = item {
            let child_dir = dir.join(&m.name);
            match &mut m.items {
                // Inline module: only its file submodules need resolving.
                Some(inner) => resolve_mods(inner, &child_dir, diags),
                // File module: load `dir/name.rune`, then resolve its submodules
                // under `dir/name/`.
                None => {
                    let file = dir.join(format!("{}.rune", m.name));
                    match load_module_file(&file) {
                        Ok(mut loaded) => {
                            resolve_mods(&mut loaded, &child_dir, diags);
                            m.items = Some(loaded);
                        }
                        Err(mut ds) => diags.append(&mut ds),
                    }
                }
            }
        }
    }
}

fn load_module_file(file: &Path) -> Result<Vec<ast::Item>, Vec<Diagnostic>> {
    let src = std::fs::read_to_string(file).map_err(|e| {
        vec![Diagnostic::new(
            Stage::Parse,
            format!("cannot read module file `{}`: {}", file.display(), e),
            Span::dummy(),
        )]
    })?;
    Ok(parse_source(&src, file)?.items)
}

/// Locate the standard-library directory, if available, honouring the
/// `RUNE_STD` environment variable and falling back to a `std/` directory
/// relative to `near` (e.g. the entry file or current dir). Returns `None` when
/// no std directory exists.
pub fn find_std_dir(near: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RUNE_STD") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(n) = near {
        if let Some(parent) = n.parent() {
            candidates.push(parent.join("std"));
        }
    }
    candidates.push(PathBuf::from("std"));
    candidates.into_iter().find(|p| p.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::Interpreter;

    #[test]
    fn loads_file_module() {
        // Build a tiny two-file project in a unique temp directory.
        let dir = std::env::temp_dir().join(format!("rune_loader_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("util.rune"),
            "fn triple(x: u32) -> u32 { x * 3 }\n",
        )
        .unwrap();
        let entry = dir.join("main.rune");
        std::fs::write(
            &entry,
            "mod util;\nfn main() { print(util::triple(14)); }\n",
        )
        .unwrap();

        let module = compile_path(&entry).expect("compiles");
        assert!(module.funcs.contains_key("util::triple"));
        let out = Interpreter::new(module).run_main().expect("runs");
        assert_eq!(out, vec!["42"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_module_file_is_reported() {
        let dir = std::env::temp_dir().join(format!("rune_loader_missing_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entry = dir.join("main.rune");
        std::fs::write(&entry, "mod nope;\nfn main() {}\n").unwrap();
        let err = compile_path(&entry).unwrap_err();
        assert!(err[0].message.contains("nope.rune"), "{}", err[0].message);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

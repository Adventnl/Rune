//! # Hot reload (Phase 2A)
//!
//! This module makes a running Rune program *swappable*: you can edit the
//! source, recompile, and splice the new definitions into a live session
//! **without losing runtime state**.
//!
//! ## Definitions vs. live state
//!
//! The key idea is a hard split between two kinds of things:
//!
//! - **Definitions** — functions and named types (`struct`/`enum`). These live
//!   inside an [`ir::Module`] and are *pure*: they describe behaviour and
//!   shapes, but hold no running data. Definitions are freely swappable. When
//!   you reload, the whole module of definitions is replaced by the freshly
//!   typechecked one.
//!
//! - **Live runtime state** — named [`Value`]s, the moral equivalent of REPL
//!   variables. These are the data the session has accumulated and *must
//!   survive* a reload, because re-deriving them may be expensive or
//!   impossible. The engine owns them in a side map keyed by name.
//!
//! Reloading definitions is always safe — code is code. The interesting part is
//! deciding whether *existing live state* is still valid against the *new*
//! definitions. A live value of a `struct`/`enum` type whose **shape changed**
//! (different fields/variants) can no longer be trusted to match the new type,
//! so it is dropped (and reported). Primitive values (`Bool`/`Int`/`Bit`) never
//! depend on a named definition, so they always survive. Arrays survive iff all
//! their elements do.
//!
//! ## Reporting, never crashing
//!
//! [`ReloadEngine::reload_str`] never panics. A compile error in the new source
//! is *rejected*: the old module and all live state are kept untouched, and the
//! errors are returned in [`ReloadReport::errors`] with `applied == false`. A
//! successful reload returns a [`ReloadReport`] describing exactly what changed:
//! which functions/types were added, removed, had their bodies swapped, had
//! breaking signature/shape changes, and which live values had to be dropped.
//! Breaking changes are *reported, not refused* — the new module is still
//! installed, so the session can carry on with the caller's eyes open.
//!
//! ## File watching
//!
//! [`FileWatcher`] is a dependency-free polling watcher (mtime based) so a REPL
//! loop can cheaply detect that a file on disk was edited and trigger a reload
//! via [`ReloadEngine::reload_file`].

use crate::interp::{Interpreter, Value};
use crate::ir;
use crate::{Diagnostic, Stage};
use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

/// A summary of what a single [`ReloadEngine::reload_str`] did. Every field is a
/// plain, sorted list of names (or diagnostics) so callers can render it however
/// they like.
#[derive(Clone, Debug, Default)]
pub struct ReloadReport {
    /// Functions/types present only in the new source.
    pub added: Vec<String>,
    /// Functions/types present only in the old module.
    pub removed: Vec<String>,
    /// Functions whose body changed but whose signature is identical — swapped
    /// transparently.
    pub changed: Vec<String>,
    /// Functions swapped, but with a *different* signature (a breaking change
    /// for any caller relying on the old shape).
    pub signature_changed: Vec<String>,
    /// Structs/enums present in both whose definition (shape) changed.
    pub type_changed: Vec<String>,
    /// Live variables dropped because the named type they depend on changed
    /// shape.
    pub dropped_state: Vec<String>,
    /// Compile errors in the new source. When non-empty the reload was
    /// **rejected**: the old module and all live state are kept.
    pub errors: Vec<Diagnostic>,
    /// True iff the new module was installed.
    pub applied: bool,
}

/// Owns the current typed module (the swappable definitions) plus the live named
/// values that must survive across reloads.
pub struct ReloadEngine {
    module: ir::Module,
    live: BTreeMap<String, Value>,
}

impl ReloadEngine {
    /// Build an engine from initial source. Returns the front-end diagnostics on
    /// a compile failure.
    pub fn new(src: &str) -> Result<ReloadEngine, Vec<Diagnostic>> {
        let module = crate::compile(src)?;
        Ok(ReloadEngine {
            module,
            live: BTreeMap::new(),
        })
    }

    /// The module currently installed.
    pub fn module(&self) -> &ir::Module {
        &self.module
    }

    /// Build a fresh [`Interpreter`] over the current module and run `main()`.
    pub fn run_main(&mut self) -> Result<Vec<String>, Diagnostic> {
        let mut interp = Interpreter::new(self.module.clone());
        interp.run_main()
    }

    /// Call a function by name on the current module.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, Diagnostic> {
        let mut interp = Interpreter::new(self.module.clone());
        interp.call(name, args)
    }

    /// Insert or replace a live named value.
    pub fn set_live(&mut self, name: &str, value: Value) {
        self.live.insert(name.to_string(), value);
    }

    /// Look up a live named value.
    pub fn live(&self, name: &str) -> Option<&Value> {
        self.live.get(name)
    }

    /// All live variable names, sorted.
    pub fn live_names(&self) -> Vec<String> {
        self.live.keys().cloned().collect()
    }

    /// Recompile `new_src` and, if it typechecks, splice its definitions into
    /// the session, preserving compatible live state. See the module docs for
    /// the full contract. Never panics.
    pub fn reload_str(&mut self, new_src: &str) -> ReloadReport {
        let mut report = ReloadReport::default();

        // 1. Compile the new source. On failure, reject the reload outright and
        //    keep the old module + all live state.
        let new_module = match crate::compile(new_src) {
            Ok(m) => m,
            Err(diags) => {
                report.errors = diags;
                report.applied = false;
                return report;
            }
        };

        let old = &self.module;

        // 2. Diff functions.
        let old_funcs: BTreeSet<&String> = old.funcs.keys().collect();
        let new_funcs: BTreeSet<&String> = new_module.funcs.keys().collect();
        for name in new_funcs.difference(&old_funcs) {
            report.added.push((*name).clone());
        }
        for name in old_funcs.difference(&new_funcs) {
            report.removed.push((*name).clone());
        }
        for name in old_funcs.intersection(&new_funcs) {
            let of = &old.funcs[*name];
            let nf = &new_module.funcs[*name];
            if of.signature() != nf.signature() {
                report.signature_changed.push((*name).clone());
            } else if of.body != nf.body {
                report.changed.push((*name).clone());
            }
        }

        // 2b. Diff named types (structs + enums share one name space for the
        //     purposes of added/removed/type_changed reporting).
        let mut old_types: BTreeSet<String> = BTreeSet::new();
        let mut new_types: BTreeSet<String> = BTreeSet::new();
        old_types.extend(old.structs.keys().cloned());
        old_types.extend(old.enums.keys().cloned());
        new_types.extend(new_module.structs.keys().cloned());
        new_types.extend(new_module.enums.keys().cloned());

        for name in new_types.difference(&old_types) {
            report.added.push(name.clone());
        }
        for name in old_types.difference(&new_types) {
            report.removed.push(name.clone());
        }

        // A named type changed shape if its definition differs. A type that
        // switched between struct<->enum also counts as changed.
        let mut changed_types: BTreeSet<String> = BTreeSet::new();
        for name in old_types.intersection(&new_types) {
            if !type_defs_equal(old, &new_module, name) {
                changed_types.insert(name.clone());
            }
        }
        report.type_changed = changed_types.iter().cloned().collect();

        // 3. Determine which live values survive against the new definitions.
        let mut surviving: BTreeMap<String, Value> = BTreeMap::new();
        for (name, value) in &self.live {
            if value_depends_on_changed_type(value, &changed_types) {
                report.dropped_state.push(name.clone());
            } else {
                surviving.insert(name.clone(), value.clone());
            }
        }

        // 4. Install the new module and the surviving live state.
        self.module = new_module;
        self.live = surviving;
        report.applied = true;

        // Stable, readable output.
        report.added.sort();
        report.added.dedup();
        report.removed.sort();
        report.removed.dedup();
        report.changed.sort();
        report.signature_changed.sort();
        report.dropped_state.sort();

        report
    }

    /// Read `path` and reload from its contents.
    pub fn reload_file<P: AsRef<std::path::Path>>(
        &mut self,
        path: P,
    ) -> std::io::Result<ReloadReport> {
        let src = std::fs::read_to_string(path)?;
        Ok(self.reload_str(&src))
    }
}

/// True if the named type has the *same* definition in both modules. A type that
/// exists as a struct in one and an enum in the other is considered different.
fn type_defs_equal(old: &ir::Module, new: &ir::Module, name: &str) -> bool {
    match (old.structs.get(name), new.structs.get(name)) {
        (Some(a), Some(b)) => return a == b,
        (Some(_), None) | (None, Some(_)) => {
            // struct on one side; check it isn't an enum on the other.
            // If the other side has it as an enum, they differ.
        }
        (None, None) => {}
    }
    match (old.enums.get(name), new.enums.get(name)) {
        (Some(a), Some(b)) => return a == b,
        (Some(_), None) | (None, Some(_)) => return false,
        (None, None) => {}
    }
    // If we get here it means the kind switched (struct<->enum) — different.
    false
}

/// Does this live value (transitively) depend on a named type whose shape
/// changed? Primitives never do; structs/enums depend on their own name;
/// arrays recurse into their elements.
fn value_depends_on_changed_type(value: &Value, changed: &BTreeSet<String>) -> bool {
    match value {
        Value::Bool(_) | Value::Int(_, _) | Value::Bit(_, _) | Value::Unit => false,
        Value::Array(elems) => elems
            .iter()
            .any(|e| value_depends_on_changed_type(e, changed)),
        Value::Struct(name, fields) => {
            changed.contains(name)
                || fields
                    .iter()
                    .any(|f| value_depends_on_changed_type(f, changed))
        }
        Value::Enum(name, _tag, args) => {
            changed.contains(name)
                || args
                    .iter()
                    .any(|a| value_depends_on_changed_type(a, changed))
        }
        Value::Tuple(elems) => elems
            .iter()
            .any(|e| value_depends_on_changed_type(e, changed)),
    }
}

/// A dependency-free polling file watcher. It records the file's last-modified
/// time and reports when that time advances. This is deliberately simple: a REPL
/// loop can poll [`changed`](FileWatcher::changed) on an interval and trigger a
/// reload when it returns `true`.
pub struct FileWatcher {
    path: std::path::PathBuf,
    last: Option<SystemTime>,
}

impl FileWatcher {
    /// Start watching `path`. The current mtime (if the file exists) becomes the
    /// baseline, so the first [`changed`](FileWatcher::changed) only reports
    /// `true` after an actual edit.
    pub fn new<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<FileWatcher> {
        let path = path.as_ref().to_path_buf();
        let last = mtime(&path).ok();
        Ok(FileWatcher { path, last })
    }

    /// Returns `Ok(true)` if the file's mtime advanced since the previous call
    /// (or since construction). Updates the stored baseline on every call.
    pub fn changed(&mut self) -> std::io::Result<bool> {
        let now = mtime(&self.path)?;
        let changed = match self.last {
            Some(prev) => now > prev,
            None => true,
        };
        self.last = Some(now);
        Ok(changed)
    }

    /// Read the watched file's current contents.
    pub fn read(&self) -> std::io::Result<String> {
        std::fs::read_to_string(&self.path)
    }
}

fn mtime(path: &std::path::Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}

/// Helper to wrap an io error as a reload diagnostic (handy for REPL callers).
#[allow(dead_code)]
fn io_diag(e: &std::io::Error) -> Diagnostic {
    Diagnostic::new(
        Stage::Reload,
        format!("file watch error: {}", e),
        crate::Span::dummy(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::IntTy;

    fn u32t() -> IntTy {
        IntTy {
            signed: false,
            width: 32,
        }
    }

    #[test]
    fn behavior_change_preserves_live_state() {
        let mut engine = ReloadEngine::new("fn f() -> u32 { 1 }").unwrap();
        engine.set_live("x", Value::Int(42, u32t()));
        assert_eq!(engine.call("f", vec![]).unwrap(), Value::Int(1, u32t()));

        let report = engine.reload_str("fn f() -> u32 { 2 }");
        assert!(report.applied);
        assert!(report.changed.contains(&"f".to_string()));
        assert!(report.signature_changed.is_empty());
        // Live state survived a behaviour-only change.
        assert_eq!(engine.live("x"), Some(&Value::Int(42, u32t())));
        // New behaviour is in effect.
        assert_eq!(engine.call("f", vec![]).unwrap(), Value::Int(2, u32t()));
    }

    #[test]
    fn signature_change_is_reported_not_crashed() {
        let mut engine = ReloadEngine::new("fn f() -> u32 { 1 }").unwrap();
        let report = engine.reload_str("fn f(a: u32) -> u32 { a }");
        assert!(report.applied);
        assert!(report.signature_changed.contains(&"f".to_string()));
        assert!(report.changed.is_empty());
        // The new arity is callable; no panic.
        assert_eq!(
            engine.call("f", vec![Value::Int(7, u32t())]).unwrap(),
            Value::Int(7, u32t())
        );
    }

    #[test]
    fn type_shape_change_drops_dependent_state() {
        let src = r#"
            struct S { a: u32 }
            fn make() -> S { S { a: 1 } }
        "#;
        let mut engine = ReloadEngine::new(src).unwrap();
        // A live struct value of type S, and a primitive that must survive.
        engine.set_live("s", Value::Struct("S".to_string(), vec![Value::Int(9, u32t())]));
        engine.set_live("p", Value::Int(5, u32t()));

        let new_src = r#"
            struct S { a: u32, b: u32 }
            fn make() -> S { S { a: 1, b: 2 } }
        "#;
        let report = engine.reload_str(new_src);

        assert!(report.applied);
        assert!(report.type_changed.contains(&"S".to_string()));
        assert!(report.dropped_state.contains(&"s".to_string()));
        // Primitive survives.
        assert_eq!(engine.live("p"), Some(&Value::Int(5, u32t())));
        assert_eq!(engine.live("s"), None);
    }

    #[test]
    fn array_of_changed_struct_is_dropped() {
        let src = r#"
            struct S { a: u32 }
            fn make() -> S { S { a: 1 } }
        "#;
        let mut engine = ReloadEngine::new(src).unwrap();
        engine.set_live(
            "arr",
            Value::Array(vec![Value::Struct(
                "S".to_string(),
                vec![Value::Int(1, u32t())],
            )]),
        );
        let new_src = r#"
            struct S { a: u32, b: u32 }
            fn make() -> S { S { a: 1, b: 2 } }
        "#;
        let report = engine.reload_str(new_src);
        assert!(report.dropped_state.contains(&"arr".to_string()));
        assert_eq!(engine.live("arr"), None);
    }

    #[test]
    fn compile_error_is_rejected_and_old_kept() {
        let mut engine = ReloadEngine::new("fn f() -> u32 { 1 }").unwrap();
        engine.set_live("x", Value::Int(42, u32t()));

        // Type error: returning a bool where u32 is expected.
        let report = engine.reload_str("fn f() -> u32 { true }");
        assert!(!report.errors.is_empty());
        assert!(!report.applied);
        // Old module untouched: f still returns 1, live state intact.
        assert_eq!(engine.call("f", vec![]).unwrap(), Value::Int(1, u32t()));
        assert_eq!(engine.live("x"), Some(&Value::Int(42, u32t())));
    }

    #[test]
    fn syntax_error_is_rejected() {
        let mut engine = ReloadEngine::new("fn f() -> u32 { 1 }").unwrap();
        let report = engine.reload_str("fn f( {{{ this is not rune");
        assert!(!report.errors.is_empty());
        assert!(!report.applied);
        assert_eq!(engine.call("f", vec![]).unwrap(), Value::Int(1, u32t()));
    }

    #[test]
    fn added_and_removed_functions_are_reported() {
        let mut engine = ReloadEngine::new("fn f() -> u32 { 1 }").unwrap();
        let report = engine.reload_str("fn g() -> u32 { 2 }");
        assert!(report.added.contains(&"g".to_string()));
        assert!(report.removed.contains(&"f".to_string()));
        assert!(report.applied);
    }

    #[test]
    fn file_watcher_detects_edits() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "rune_hotreload_watch_{}_{}.rune",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        std::fs::write(&path, "fn f() -> u32 { 1 }").unwrap();
        let mut watcher = FileWatcher::new(&path).unwrap();
        // No edit yet.
        assert_eq!(watcher.changed().unwrap(), false);

        // Sleep past the filesystem's timestamp resolution, then edit so the
        // mtime is guaranteed to advance regardless of clock granularity.
        std::thread::sleep(std::time::Duration::from_millis(20));
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            f.write_all(b"fn f() -> u32 { 2 }").unwrap();
            f.flush().unwrap();
        }

        assert!(watcher.changed().unwrap());
        assert_eq!(watcher.read().unwrap(), "fn f() -> u32 { 2 }");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reload_file_round_trips() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "rune_hotreload_file_{}_{}.rune",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "fn f() -> u32 { 1 }").unwrap();
        let mut engine = ReloadEngine::new("fn f() -> u32 { 0 }").unwrap();
        let report = engine.reload_file(&path).unwrap();
        assert!(report.applied);
        assert_eq!(engine.call("f", vec![]).unwrap(), Value::Int(1, u32t()));
        let _ = std::fs::remove_file(&path);
    }
}

//! Integration tests for hot reload, exercising the public API the way a REPL
//! front-end would (from outside the crate).

use rune::hotreload::{FileWatcher, ReloadEngine};
use rune::interp::Value;
use rune::ir::IntTy;

fn u32t() -> IntTy {
    IntTy {
        signed: false,
        width: 32,
    }
}

#[test]
fn behavior_change_with_preserved_state() {
    let mut engine = ReloadEngine::new("fn f() -> u32 { 1 }").unwrap();
    engine.set_live("x", Value::Int(42, u32t()));

    let report = engine.reload_str("fn f() -> u32 { 2 }");
    assert!(report.applied);
    assert!(report.changed.contains(&"f".to_string()));
    assert_eq!(engine.live("x"), Some(&Value::Int(42, u32t())));
    assert_eq!(engine.call("f", vec![]).unwrap(), Value::Int(2, u32t()));
}

#[test]
fn incompatible_signature_change_reported() {
    let mut engine = ReloadEngine::new("fn f() -> u32 { 1 }").unwrap();
    let report = engine.reload_str("fn f(a: u32) -> u32 { a }");
    assert!(report.applied);
    assert!(report.signature_changed.contains(&"f".to_string()));
}

#[test]
fn type_shape_change_drops_state() {
    let mut engine = ReloadEngine::new("struct S { a: u32 } fn g() -> u32 { 0 }").unwrap();
    engine.set_live(
        "s",
        Value::Struct("S".to_string(), vec![Value::Int(1, u32t())]),
    );
    engine.set_live("p", Value::Bool(true));

    let report = engine.reload_str("struct S { a: u32, b: u32 } fn g() -> u32 { 0 }");
    assert!(report.type_changed.contains(&"S".to_string()));
    assert!(report.dropped_state.contains(&"s".to_string()));
    assert_eq!(engine.live("p"), Some(&Value::Bool(true)));
    assert_eq!(engine.live("s"), None);
}

#[test]
fn compile_error_rejected_old_kept() {
    let mut engine = ReloadEngine::new("fn f() -> u32 { 1 }").unwrap();
    let report = engine.reload_str("fn f() -> u32 { true }");
    assert!(!report.errors.is_empty());
    assert!(!report.applied);
    assert_eq!(engine.call("f", vec![]).unwrap(), Value::Int(1, u32t()));
}

#[test]
fn file_watcher_flips_on_edit() {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!(
        "rune_it_watch_{}_{}.rune",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, "fn f() -> u32 { 1 }").unwrap();
    let mut watcher = FileWatcher::new(&path).unwrap();
    assert!(!watcher.changed().unwrap());

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
    let _ = std::fs::remove_file(&path);
}

//! Integration tests for the bundled standard library (`std/*.rune`).
//!
//! These point `RUNE_STD` at the workspace `std/` directory and compile small
//! programs that `use` the library, asserting both that it compiles and that it
//! computes the right values.

use std::path::{Path, PathBuf};

fn std_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = <workspace>/crates/rune ; std/ is at the workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../std")
        .canonicalize()
        .expect("std dir exists")
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Compile + run a program with the std library injected. Each call uses a
/// unique temp directory so parallel tests don't race.
fn run_with_std(src: &str) -> Vec<String> {
    std::env::set_var("RUNE_STD", std_dir());
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("rune_std_test_{}_{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("prog.rune");
    std::fs::write(&entry, src).unwrap();
    let module = rune::compile_path(&entry).expect("compiles with std");
    let out = rune::Interpreter::new(module).run_main().expect("runs");
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[test]
fn math_helpers() {
    let out = run_with_std(
        r#"
        use std::math::pow_u32;
        fn main() {
            print(std::math::min_u32(7, 3));
            print(std::math::max_u32(7, 3));
            print(std::math::clamp_u32(99, 0, 64));
            print(std::math::gcd_u32(48, 36));
            print(pow_u32(2, 10));
            print(std::math::abs_i32(-9));
            print(std::math::sum_to_u32(5));
        }
        "#,
    );
    assert_eq!(out, vec!["3", "7", "64", "12", "1024", "9", "10"]);
}

#[test]
fn bit_helpers() {
    let out = run_with_std(
        r#"
        use std::bits::popcount32;
        fn main() {
            print(popcount32(0xFF));
            print(std::bits::parity32(0x7));
            print(std::bits::reverse32(1));
            print(std::bits::rotl32(0x1, 4));
            print(std::bits::rotr32(0x10, 4));
            print(std::bits::get_bit32(0x4, 2));
        }
        "#,
    );
    assert_eq!(out, vec!["8", "1", "2147483648", "16", "1", "1"]);
}

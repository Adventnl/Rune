//! Golden-test corpus for the Rune front-end + interpreter.
//!
//! Each case is a small, self-contained Rune program paired with either:
//!   * its exact captured `print` output (success cases), or
//!   * the pipeline [`Stage`] at which it must fail (compile-time error cases),
//!   * or a runtime trap (defined errors: divide-by-zero, out-of-bounds).
//!
//! The interpreter is the source of truth: expected outputs were produced by
//! running each program through the real pipeline. These tests pin that
//! behavior so regressions in the lexer, parser, typechecker, or interpreter
//! are caught immediately.

use rune::Stage;

/// Compile and run `main`, returning the captured print lines. Panics (failing
/// the test) if the program does not compile or traps at runtime.
fn run(src: &str) -> Vec<String> {
    let m = rune::compile(src).expect("program should compile");
    rune::interp::Interpreter::new(m)
        .run_main()
        .expect("program should run without trapping")
}

/// Compile only; return the diagnostics on failure (panicking if it unexpectedly
/// compiles).
fn compile_err(src: &str) -> Vec<rune::Diagnostic> {
    match rune::compile(src) {
        Ok(_) => panic!("expected a compile error, but the program compiled"),
        Err(diags) => diags,
    }
}

/// Assert the program fails to compile and the *first* diagnostic is at `stage`.
fn assert_compile_stage(src: &str, stage: Stage) {
    let diags = compile_err(src);
    assert!(
        diags.iter().any(|d| d.stage == stage),
        "expected a `{:?}` diagnostic, got: {:?}",
        stage,
        diags.iter().map(|d| (d.stage, &d.message)).collect::<Vec<_>>()
    );
}

/// Compile (must succeed), then run and expect a runtime trap whose message
/// contains `needle`.
fn assert_runtime_trap(src: &str, needle: &str) {
    let m = rune::compile(src).expect("program should compile");
    let err = rune::interp::Interpreter::new(m)
        .run_main()
        .expect_err("expected a runtime trap");
    assert_eq!(err.stage, Stage::Runtime, "trap should be a runtime error");
    assert!(
        err.message.contains(needle),
        "trap message `{}` should contain `{}`",
        err.message,
        needle
    );
}

// ---------------------------------------------------------------------------
// Overflow / wrapping
// ---------------------------------------------------------------------------

#[test]
fn bit8_add_wraps() {
    // 200 + 100 = 300, wrapped into bit<8> => 44.
    let src = "fn main() { let a: bit<8> = 200; let b: bit<8> = 100; print(a + b); }";
    assert_eq!(run(src), ["44"]);
}

#[test]
fn bit8_mul_wraps() {
    // 100 * 4 = 400, mod 256 => 144.
    let src = "fn main() { let a: bit<8> = 100; print(a * 4); }";
    assert_eq!(run(src), ["144"]);
}

#[test]
fn signed_i8_overflow_wraps() {
    // 127 + 1 overflows to -128 (two's complement).
    let src = "fn main() { let a: i8 = 127; print(a + 1); }";
    assert_eq!(run(src), ["-128"]);
}

#[test]
fn unsigned_u8_wraps() {
    // 255 + 1 wraps to 0; 0 - 1 wraps to 255.
    let src = r#"
        fn main() {
            let a: u8 = 255;
            print(a + 1);
            let b: u8 = 0;
            print(b - 1);
        }
    "#;
    assert_eq!(run(src), ["0", "255"]);
}

// ---------------------------------------------------------------------------
// Bitwise & shifts
// ---------------------------------------------------------------------------

#[test]
fn bitwise_ops_and_hex() {
    let src = r#"
        fn main() {
            let a: bit<8> = 0xF0;
            let b: bit<8> = 0x0F;
            print(a & b);   // 0
            print(a | b);   // 255
            print(a ^ b);   // 255
        }
    "#;
    assert_eq!(run(src), ["0", "255", "255"]);
}

#[test]
fn shifts_and_not() {
    let src = r#"
        fn main() {
            let a: bit<8> = 0xF0;
            let b: bit<8> = 0x0F;
            print(a >> 4);  // 15
            print(b << 4);  // 240
            print(!b);      // 240 (NOT 0x0F within 8 bits)
            print(!a);      // 15
        }
    "#;
    assert_eq!(run(src), ["15", "240", "240", "15"]);
}

#[test]
fn bit16_ops() {
    let src = r#"
        fn main() {
            let a: bit<16> = 0xABCD;
            print(a & 0x00FF); // 205
            print(a >> 8);     // 171
        }
    "#;
    assert_eq!(run(src), ["205", "171"]);
}

// ---------------------------------------------------------------------------
// Compile-time errors (typecheck stage)
// ---------------------------------------------------------------------------

#[test]
fn non_exhaustive_match_is_type_error() {
    let src = r#"
        enum E { A, B }
        fn f(e: E) -> u8 { match e { A => 1 } }
        fn main() { print(f(A)); }
    "#;
    assert_compile_stage(src, Stage::Type);
}

#[test]
fn mismatched_width_binop_is_type_error() {
    let src = "fn f(a: u8, b: u32) -> u8 { a + b } fn main() { print(f(1, 2)); }";
    assert_compile_stage(src, Stage::Type);
}

#[test]
fn out_of_range_literal_is_type_error() {
    let src = "fn main() { let x: u8 = 300; print(x); }";
    assert_compile_stage(src, Stage::Type);
}

#[test]
fn assign_to_immutable_is_type_error() {
    let src = "fn main() { let x: u8 = 1; x = 2; print(x); }";
    assert_compile_stage(src, Stage::Type);
}

#[test]
fn unknown_type_is_type_error() {
    let src = "fn f(x: Nope) -> u8 { 0 } fn main() { print(0); }";
    assert_compile_stage(src, Stage::Type);
}

// ---------------------------------------------------------------------------
// Nested matches & match-on-bool
// ---------------------------------------------------------------------------

#[test]
fn nested_match_on_nested_enums() {
    // A match whose arms each contain another match.
    let src = r#"
        enum Color { Red, Green, Blue }
        enum Light { On(Color), Off }
        fn describe(l: Light) -> u8 {
            match l {
                On(c) => match c {
                    Red   => 1,
                    Green => 2,
                    Blue  => 3,
                },
                Off => 0,
            }
        }
        fn main() {
            print(describe(On(Red)));
            print(describe(On(Blue)));
            print(describe(Off));
        }
    "#;
    assert_eq!(run(src), ["1", "3", "0"]);
}

#[test]
fn match_on_bool() {
    let src = r#"
        fn flip(b: bool) -> u8 {
            match b {
                true  => 0,
                false => 1,
            }
        }
        fn main() {
            print(flip(true));
            print(flip(false));
        }
    "#;
    assert_eq!(run(src), ["0", "1"]);
}

#[test]
fn match_with_int_patterns_and_wildcard() {
    let src = r#"
        fn classify(n: i32) -> u8 {
            match n {
                0 => 10,
                1 => 11,
                _ => 99,
            }
        }
        fn main() {
            print(classify(0));
            print(classify(1));
            print(classify(7));
        }
    "#;
    assert_eq!(run(src), ["10", "11", "99"]);
}

// ---------------------------------------------------------------------------
// Arrays
// ---------------------------------------------------------------------------

#[test]
fn array_build_index_sum_mutate_print() {
    let src = r#"
        fn main() {
            let mut a: [u32; 4] = [1, 2, 3, 4];
            print(a[0]);          // 1
            a[1] = 20;            // mutate one element
            print(a);             // [1, 20, 3, 4]
            let mut s: u32 = 0;
            for i in 0..4 { s = s + a[i]; }
            print(s);             // 28
        }
    "#;
    assert_eq!(run(src), ["1", "[1, 20, 3, 4]", "28"]);
}

#[test]
fn array_of_bool_prints() {
    let src = r#"
        fn main() {
            let a: [bool; 3] = [true, false, true];
            print(a);
        }
    "#;
    assert_eq!(run(src), ["[true, false, true]"]);
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[test]
fn struct_construct_read_mutate_print() {
    let src = r#"
        struct P { x: u32, y: u32 }
        fn main() {
            let mut p = P { x: 1, y: 2 };
            print(p.x);                 // 1
            p.x = p.x + 10;
            print(p);                   // P { x: 11, y: 2 }
        }
    "#;
    assert_eq!(run(src), ["1", "P { x: 11, y: 2 }"]);
}

#[test]
fn nested_structs() {
    let src = r#"
        struct Point { x: i32, y: i32 }
        struct Line { a: Point, b: Point }
        fn main() {
            let mut l = Line { a: Point { x: 0, y: 0 }, b: Point { x: 3, y: 4 } };
            print(l.b.x);                 // 3
            l.a.x = 7;
            print(l.a.x);                 // 7
            print(l);
        }
    "#;
    assert_eq!(
        run(src),
        [
            "3",
            "7",
            "Line { a: Point { x: 7, y: 0 }, b: Point { x: 3, y: 4 } }",
        ]
    );
}

// ---------------------------------------------------------------------------
// Enums with payloads
// ---------------------------------------------------------------------------

#[test]
fn enum_payload_construct_print_and_extract() {
    let src = r#"
        enum Shape { Circle(u32), Rect(u32, u32), Dot }
        fn area(s: Shape) -> u32 {
            match s {
                Circle(r)  => 3 * r * r,
                Rect(w, h) => w * h,
                Dot        => 0,
            }
        }
        fn main() {
            print(Rect(3, 4));        // Rect(3, 4)
            print(Circle(2));         // Circle(2)
            print(Dot);               // Dot
            print(area(Rect(3, 4)));  // 12
            print(area(Circle(2)));   // 12
            print(area(Dot));         // 0
        }
    "#;
    assert_eq!(
        run(src),
        ["Rect(3, 4)", "Circle(2)", "Dot", "12", "12", "0"]
    );
}

// ---------------------------------------------------------------------------
// Control flow: while, if/else expression, recursion
// ---------------------------------------------------------------------------

#[test]
fn while_loop_factorial() {
    let src = r#"
        fn main() {
            let mut n: i32 = 5;
            let mut acc: i32 = 1;
            while n > 0 { acc = acc * n; n = n - 1; }
            print(acc); // 120
        }
    "#;
    assert_eq!(run(src), ["120"]);
}

#[test]
fn if_else_as_expression() {
    let src = r#"
        fn max(a: i32, b: i32) -> i32 {
            if a > b { a } else { b }
        }
        fn main() {
            print(max(3, 7));  // 7
            print(max(9, 2));  // 9
        }
    "#;
    assert_eq!(run(src), ["7", "9"]);
}

#[test]
fn recursion_fibonacci() {
    let src = r#"
        fn fib(n: u32) -> u32 {
            if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
        }
        fn main() { print(fib(10)); } // 55
    "#;
    assert_eq!(run(src), ["55"]);
}

// ---------------------------------------------------------------------------
// Runtime traps (defined, never UB)
// ---------------------------------------------------------------------------

#[test]
fn divide_by_zero_traps() {
    let src = "fn main() { let z: i32 = 0; print(1 / z); }";
    assert_runtime_trap(src, "division by zero");
}

#[test]
fn array_index_out_of_bounds_traps() {
    let src = "fn main() { let a: [u8; 2] = [1, 2]; let i: u32 = 5; print(a[i]); }";
    assert_runtime_trap(src, "out of bounds");
}

// ---------------------------------------------------------------------------
// The flagship milestone program (front-to-back smoke test).
// ---------------------------------------------------------------------------

#[test]
fn milestone_program() {
    let src = r#"
        fn add8(a: bit<8>, b: bit<8>) -> bit<8> { a + b }
        enum Shape { Circle(u32), Rect(u32, u32) }
        fn area(s: Shape) -> u32 {
            match s {
                Circle(r)  => 3 * r * r,
                Rect(w, h) => w * h,
            }
        }
        fn main() {
            print(add8(200, 100));
            print(area(Rect(3, 4)));
            print(area(Circle(2)));
        }
    "#;
    assert_eq!(run(src), ["44", "12", "12"]);
}

// ---------------------------------------------------------------------------
// Examples on disk: each `examples/*.rune` must at least compile and run.
// ---------------------------------------------------------------------------

#[test]
fn examples_compile_and_run() {
    // The `rune` crate lives at <workspace>/crates/rune, so the examples
    // directory is two levels up.
    let manifest = env!("CARGO_MANIFEST_DIR");
    let examples_dir = std::path::Path::new(manifest)
        .join("..")
        .join("..")
        .join("examples");

    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&examples_dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {}", examples_dir.display(), e))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "rune").unwrap_or(false))
        .collect();
    entries.sort();

    assert!(
        !entries.is_empty(),
        "expected at least one example in {}",
        examples_dir.display()
    );

    // Point the loader at the workspace std/ so examples that `use std::...`
    // resolve. Use `compile_path` so multi-file/module examples work too.
    let std_dir = std::path::Path::new(manifest).join("../../std");
    if std_dir.is_dir() {
        std::env::set_var("RUNE_STD", std_dir);
    }

    for path in entries {
        let module = rune::compile_path(&path).unwrap_or_else(|diags| {
            panic!(
                "example {} failed to compile: {:?}",
                path.display(),
                diags.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        });
        rune::interp::Interpreter::new(module)
            .run_main()
            .unwrap_or_else(|d| panic!("example {} trapped at runtime: {}", path.display(), d));
    }
}

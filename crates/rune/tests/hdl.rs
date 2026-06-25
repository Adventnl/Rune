//! Integration tests for the HDL-subset analysis pass (`rune::hdl`).
//!
//! These compile real Rune source to typed IR and assert the exact
//! synthesizability classification, including the *reasons* attached to
//! rejected functions.

use rune::hdl::{analyze, report_string, FunctionReport};

/// Compile source to a module, panicking with diagnostics on failure.
fn module(src: &str) -> rune::ir::Module {
    match rune::compile(src) {
        Ok(m) => m,
        Err(diags) => panic!("compile failed: {:?}", diags),
    }
}

/// Find the report for a named function.
fn report<'a>(reports: &'a [FunctionReport], name: &str) -> &'a FunctionReport {
    reports
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no report for `{}`", name))
}

const MILESTONE: &str = r#"
fn add8(a: bit<8>, b: bit<8>) -> bit<8> {
    a + b
}

enum Shape { Circle(u32), Rect(u32, u32) }

fn area(s: Shape) -> u32 {
    match s {
        Circle(r)   => 3 * r * r,
        Rect(w, h)  => w * h,
    }
}

fn main() {
    print(add8(200, 100));
    print(area(Rect(3, 4)));
    print(area(Circle(2)));
}
"#;

#[test]
fn milestone_classification() {
    let m = module(MILESTONE);
    let reports = analyze(&m);

    // Sorted by name.
    let names: Vec<&str> = reports.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["add8", "area", "main"]);

    // add8: synthesizable, no reasons.
    let add8 = report(&reports, "add8");
    assert!(add8.synthesizable, "add8 should be synthesizable");
    assert!(add8.reasons.is_empty());

    // area: not synthesizable; reason mentions the machine-integer type.
    let area = report(&reports, "area");
    assert!(!area.synthesizable, "area should NOT be synthesizable");
    assert!(
        area.reasons
            .iter()
            .any(|r| r.contains("u32") || r.contains("Shape")),
        "area reasons should mention u32/Shape, got: {:?}",
        area.reasons
    );

    // main: not synthesizable; reasons mention print and/or calling area.
    let main = report(&reports, "main");
    assert!(!main.synthesizable, "main should NOT be synthesizable");
    assert!(
        main.reasons.iter().any(|r| r.contains("print"))
            || main.reasons.iter().any(|r| r.contains("area")),
        "main reasons should mention print or area, got: {:?}",
        main.reasons
    );
}

#[test]
fn invariant_reasons_empty_iff_synthesizable() {
    let m = module(MILESTONE);
    for r in analyze(&m) {
        assert_eq!(
            r.synthesizable,
            r.reasons.is_empty(),
            "reasons empty iff synthesizable violated for {}: {:?}",
            r.name,
            r
        );
    }
}

#[test]
fn while_loop_is_unbounded() {
    let src = r#"
fn countdown(n: bit<8>) -> bit<8> {
    let mut x: bit<8> = n;
    while x > 0 {
        x = x - 1;
    }
    x
}
"#;
    let m = module(src);
    let reports = analyze(&m);
    let f = report(&reports, "countdown");
    assert!(!f.synthesizable);
    assert!(
        f.reasons.iter().any(|r| r.contains("while")),
        "expected while reason, got: {:?}",
        f.reasons
    );
}

#[test]
fn direct_recursion_is_flagged() {
    let src = r#"
fn loopy(x: bit<8>) -> bit<8> {
    if x == 0 { 0 } else { loopy(x - 1) }
}
"#;
    let m = module(src);
    let reports = analyze(&m);
    let f = report(&reports, "loopy");
    assert!(!f.synthesizable);
    assert!(
        f.reasons.iter().any(|r| r.contains("recursive")),
        "expected recursive reason, got: {:?}",
        f.reasons
    );
}

#[test]
fn mutual_recursion_is_flagged() {
    let src = r#"
fn ping(x: bit<8>) -> bit<8> {
    if x == 0 { 0 } else { pong(x - 1) }
}
fn pong(x: bit<8>) -> bit<8> {
    if x == 0 { 1 } else { ping(x - 1) }
}
"#;
    let m = module(src);
    let reports = analyze(&m);
    for name in ["ping", "pong"] {
        let f = report(&reports, name);
        assert!(!f.synthesizable, "{} should be non-synthesizable", name);
        assert!(
            f.reasons.iter().any(|r| r.contains("recursive")),
            "{} expected recursive reason, got: {:?}",
            name,
            f.reasons
        );
    }
}

#[test]
fn pure_bounded_for_qualifies() {
    let src = r#"
fn sum_to(n: bit<8>) -> bit<8> {
    let mut acc: bit<8> = 0;
    for i in 0..8 {
        acc = acc + 1;
    }
    acc
}
"#;
    let m = module(src);
    let reports = analyze(&m);
    let f = report(&reports, "sum_to");
    assert!(
        f.synthesizable,
        "pure bounded-for over bit<N> should qualify, reasons: {:?}",
        f.reasons
    );
    assert!(f.reasons.is_empty());
}

#[test]
fn calling_non_synth_function_propagates() {
    // `wrap` is itself fine, but it calls `area` which is not synthesizable.
    let src = r#"
enum Shape { Circle(u32), Rect(u32, u32) }

fn area(s: Shape) -> u32 {
    match s {
        Circle(r)   => 3 * r * r,
        Rect(w, h)  => w * h,
    }
}

fn wrap(s: Shape) -> u32 {
    area(s)
}
"#;
    let m = module(src);
    let reports = analyze(&m);

    let wrap = report(&reports, "wrap");
    assert!(!wrap.synthesizable);
    // Its own signature is also non-hardware (Shape/u32), but it must at least
    // flag the non-synth callee chain.
    assert!(
        wrap.reasons
            .iter()
            .any(|r| r.contains("non-synthesizable function `area`"))
            || wrap.reasons.iter().any(|r| r.contains("Shape") || r.contains("u32")),
        "wrap reasons: {:?}",
        wrap.reasons
    );
}

#[test]
fn synth_calling_synth_qualifies() {
    let src = r#"
fn inc(a: bit<8>) -> bit<8> { a + 1 }
fn inc2(a: bit<8>) -> bit<8> { inc(inc(a)) }
"#;
    let m = module(src);
    let reports = analyze(&m);
    for name in ["inc", "inc2"] {
        let f = report(&reports, name);
        assert!(
            f.synthesizable,
            "{} should be synthesizable, reasons: {:?}",
            name, f.reasons
        );
    }
}

#[test]
fn hardware_struct_and_array_qualify() {
    let src = r#"
struct Pixel { r: bit<8>, g: bit<8>, b: bit<8> }

fn brighten(p: Pixel) -> Pixel {
    Pixel { r: p.r + 1, g: p.g + 1, b: p.b + 1 }
}

fn first(a: [bit<8>; 4]) -> bit<8> {
    a[0]
}
"#;
    let m = module(src);
    let reports = analyze(&m);
    for name in ["brighten", "first"] {
        let f = report(&reports, name);
        assert!(
            f.synthesizable,
            "{} should be synthesizable, reasons: {:?}",
            name, f.reasons
        );
    }
}

#[test]
fn report_string_renders_marks() {
    let m = module(MILESTONE);
    let reports = analyze(&m);
    let s = report_string(&reports);
    assert!(s.contains("add8"));
    assert!(s.contains("synthesizable"));
    // Rejected functions render the cross mark and "not synthesizable".
    assert!(s.contains("not synthesizable"));
    // One line per function.
    assert_eq!(s.lines().count(), reports.len());
}

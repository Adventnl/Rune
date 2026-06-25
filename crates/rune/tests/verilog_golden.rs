//! Golden + equivalence tests for the Verilog backend.
//!
//! Three layers of verification:
//! 1. **Golden snapshots** of the emitted Verilog text (via `insta`) so the
//!    generated HDL is reviewed and locked.
//! 2. A **randomized equivalence sweep**: for many synthesizable functions and
//!    many inputs, the lowered netlist (run through `verilog::eval`, which has
//!    Verilog semantics) must equal the Rune interpreter — the oracle.
//! 3. A **cosimulation** path that, when `iverilog` is installed, compiles the
//!    emitted Verilog + a generated self-checking testbench and runs it. It is
//!    skipped cleanly when no simulator is present (as in CI here).

use rune::interp::{Interpreter, Value};
use rune::verilog;
use std::collections::HashMap;

fn compile(src: &str) -> rune::ir::Module {
    rune::compile(src).expect("typechecks")
}

fn mask(width: u32) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

// ---------------------------------------------------------------- golden text

#[test]
fn golden_add8() {
    let src = "fn add8(a: bit<8>, b: bit<8>) -> bit<8> { a + b }";
    insta::assert_snapshot!("add8", verilog::emit(&compile(src)));
}

#[test]
fn golden_combinational_mux() {
    // A small combinational function exercising compare, if-expr, and bitops.
    let src = r#"
        fn alu(op: bit<2>, a: bit<8>, b: bit<8>) -> bit<8> {
            match op {
                0 => a + b,
                1 => a - b,
                2 => a & b,
                _ => a ^ b,
            }
        }
    "#;
    insta::assert_snapshot!("alu", verilog::emit(&compile(src)));
}

#[test]
fn golden_skips_non_synthesizable() {
    // The header lists skipped functions with reasons; only `pure` becomes a module.
    let src = r#"
        fn pure(a: bit<8>) -> bit<8> { a + 1 }
        fn impure(a: u32) -> u32 { a + 1 }
    "#;
    insta::assert_snapshot!("skips", verilog::emit(&compile(src)));
}

// ----------------------------------------------------- randomized equivalence

/// Tiny deterministic LCG so the sweep is reproducible without an rng crate.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 16
    }
}

/// For every input vector, assert the lowered netlist equals the interpreter.
fn sweep(src: &str, func: &str, in_widths: &[u32], out_width: u32, vectors: &[Vec<u128>]) {
    let module = compile(src);
    let design = verilog::lower(&module).design;
    let vmod = verilog::sanitize(func);
    let ir_func = &module.funcs[func];

    for v in vectors {
        let args: Vec<Value> = v
            .iter()
            .zip(in_widths)
            .map(|(&x, &w)| Value::Bit(x & mask(w), w))
            .collect();
        let mut interp = Interpreter::new(module.clone());
        let expected = match interp.call(func, args).expect("interp runs") {
            Value::Bit(x, _) => x,
            Value::Bool(b) => b as u128,
            other => panic!("unexpected value {:?}", other),
        };

        let mut inputs = HashMap::new();
        for (p, (&x, &w)) in ir_func.params.iter().zip(v.iter().zip(in_widths)) {
            inputs.insert(p.name.clone(), x & mask(w));
        }
        let actual = verilog::eval(&design, &vmod, &inputs).expect("eval netlist") & mask(out_width);
        assert_eq!(
            actual, expected,
            "mismatch {}({:?}): netlist {} vs interp {}",
            func, v, actual, expected
        );
    }
}

#[test]
fn sweep_arithmetic_wraps() {
    let src = "fn f(a: bit<8>, b: bit<8>, c: bit<8>) -> bit<8> { (a + b) * c - 1 }";
    let mut rng = Lcg(0xC0FFEE);
    let mut vecs = Vec::new();
    for _ in 0..400 {
        vecs.push(vec![
            (rng.next() as u128) & 0xFF,
            (rng.next() as u128) & 0xFF,
            (rng.next() as u128) & 0xFF,
        ]);
    }
    // Edge cases.
    vecs.push(vec![255, 255, 255]);
    vecs.push(vec![0, 0, 0]);
    sweep(src, "f", &[8, 8, 8], 8, &vecs);
}

#[test]
fn sweep_shifts_and_compare() {
    let src = r#"
        fn g(x: bit<16>, n: bit<16>) -> bit<16> {
            let s: bit<16> = x << n;
            if s > x { s } else { x >> n }
        }
    "#;
    let mut rng = Lcg(0x1234_5678);
    let mut vecs = Vec::new();
    for _ in 0..400 {
        vecs.push(vec![(rng.next() as u128) & 0xFFFF, (rng.next() as u128) & 0x1F]);
    }
    sweep(src, "g", &[16, 16], 16, &vecs);
}

#[test]
fn sweep_exhaustive_small() {
    // Fully exhaustive over an 8-bit input for a non-trivial function.
    let src = "fn h(x: bit<8>) -> bit<8> { (x ^ (x >> 1)) + 3 }"; // gray-ish
    let vecs: Vec<Vec<u128>> = (0u128..256).map(|x| vec![x]).collect();
    sweep(src, "h", &[8], 8, &vecs);
}

// --------------------------------------------------------------- cosimulation

fn iverilog_available() -> bool {
    std::process::Command::new("iverilog")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Generate a self-checking testbench: drive `cases` into the module, and
/// `$display` each output so the harness can compare against the interpreter.
fn gen_testbench(module: &verilog::Module, cases: &[Vec<u128>]) -> String {
    use std::fmt::Write;
    let mut tb = String::new();
    let inputs: Vec<&verilog::Port> = module.input_ports().collect();
    let out = module.output_port();
    let _ = writeln!(tb, "module tb;");
    for p in &inputs {
        let _ = writeln!(tb, "  reg [{}:0] {};", p.width.saturating_sub(1).max(0), p.name);
    }
    let _ = writeln!(tb, "  wire [{}:0] out;", out.width.saturating_sub(1).max(0));
    let conns: Vec<String> = inputs
        .iter()
        .map(|p| format!(".{}({})", p.name, p.name))
        .chain([format!(".out(out)")])
        .collect();
    let _ = writeln!(tb, "  {} dut ({});", module.name, conns.join(", "));
    let _ = writeln!(tb, "  initial begin");
    for case in cases {
        for (p, v) in inputs.iter().zip(case.iter()) {
            let _ = writeln!(tb, "    {} = {}'d{};", p.name, p.width, v & mask(p.width));
        }
        let _ = writeln!(tb, "    #1 $display(\"%0d\", out);");
    }
    let _ = writeln!(tb, "    $finish;\n  end\nendmodule");
    tb
}

#[test]
fn cosim_add8_if_simulator_present() {
    if !iverilog_available() {
        eprintln!("cosim skipped: iverilog not installed");
        return;
    }
    let src = "fn add8(a: bit<8>, b: bit<8>) -> bit<8> { a + b }";
    let module = compile(src);
    let design = verilog::lower(&module).design;
    let vmod = design.modules.iter().find(|m| m.name == "add8").unwrap();

    let cases: Vec<Vec<u128>> = vec![vec![200, 100], vec![255, 1], vec![0, 0], vec![17, 25]];
    let tb = gen_testbench(vmod, &cases);

    let dir = std::env::temp_dir().join(format!("rune_cosim_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("design.v"), format!("{}", design)).unwrap();
    std::fs::write(dir.join("tb.v"), tb).unwrap();

    let out_bin = dir.join("sim");
    let compile_ok = std::process::Command::new("iverilog")
        .args(["-o"])
        .arg(&out_bin)
        .arg(dir.join("design.v"))
        .arg(dir.join("tb.v"))
        .status()
        .unwrap()
        .success();
    assert!(compile_ok, "iverilog failed to compile generated Verilog");

    let run = std::process::Command::new("vvp").arg(&out_bin).output().unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout);
    let sim_results: Vec<u128> = stdout
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();

    for (case, sim) in cases.iter().zip(sim_results.iter()) {
        let mut interp = Interpreter::new(module.clone());
        let args = vec![Value::Bit(case[0], 8), Value::Bit(case[1], 8)];
        let expected = match interp.call("add8", args).unwrap() {
            Value::Bit(v, _) => v,
            _ => unreachable!(),
        };
        assert_eq!(*sim, expected, "cosim mismatch on {:?}", case);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

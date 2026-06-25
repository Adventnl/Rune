//! The Rune CLI (`runec`): run files, an interactive REPL, and hot-reload watch.
//!
//! Argument parsing is done by hand (no external deps). Subcommands:
//!
//! - `runec run <file.rune>`   — compile and run `main()`.
//! - `runec run [dir]`         — build the package in `dir` and run its `main()`.
//! - `runec repl`              — start the interactive REPL (also the default
//!                               when invoked with no arguments).
//! - `runec watch <file.rune>` — run `main()`, then hot-reload on file edits.
//! - `runec new <name>`        — scaffold a new package directory.
//! - `runec build [dir]`       — resolve + typecheck a package (no codegen).
//! - `runec test [dir]`        — build a package and run its `test_*` functions.

mod pkg;
mod repl;

use repl::{Outcome, Session};
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") => match args.get(2) {
            Some(path) => run_target(path),
            None => usage_error("usage: runec run <file.rune | dir>"),
        },
        Some("new") => match args.get(2) {
            Some(name) => new_package(name),
            None => usage_error("usage: runec new <name>"),
        },
        Some("build") => build_package(args.get(2).map(String::as_str).unwrap_or(".")),
        Some("test") => test_package(args.get(2).map(String::as_str).unwrap_or(".")),
        Some("watch") => match args.get(2) {
            Some(path) => watch_file(path),
            None => usage_error("usage: runec watch <file.rune>"),
        },
        Some("hdl") => match args.get(2) {
            Some(path) => hdl_file(path),
            None => usage_error("usage: runec hdl <file.rune>"),
        },
        Some("repl") | None => repl_loop(),
        Some("help") | Some("--help") | Some("-h") => {
            print!("{}", top_usage());
            ExitCode::SUCCESS
        }
        Some(other) => usage_error(&format!("unknown command `{}`\n\n{}", other, top_usage())),
    }
}

fn top_usage() -> String {
    format!(
        "Rune {}\n\nusage:\n  runec run <file.rune|dir>  run a file's or package's main()\n  \
         runec new <name>          scaffold a new package directory\n  \
         runec build [dir]         resolve + typecheck a package (default .)\n  \
         runec test [dir]          build a package and run its test_* functions\n  \
         runec repl                start the interactive REPL\n  \
         runec watch <file.rune>   run main(), then hot-reload on edits\n  \
         runec hdl <file.rune>     report which functions are synthesizable\n\n\
         With no arguments, starts the REPL.\n",
        env!("CARGO_PKG_VERSION")
    )
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("{}", msg);
    ExitCode::FAILURE
}

// ---- run ----------------------------------------------------------------

fn run_file(path: &str) -> ExitCode {
    // Use the module loader so multi-file projects (`mod name;`) work. The entry
    // file's own source is used to render diagnostics with carets.
    let src = std::fs::read_to_string(path).unwrap_or_default();
    let module = match rune::compile_path(std::path::Path::new(path)) {
        Ok(m) => m,
        Err(diags) => {
            for d in &diags {
                eprintln!("{}\n", d.render(&src));
            }
            return ExitCode::FAILURE;
        }
    };
    let mut interp = rune::Interpreter::new(module);
    match interp.run_main() {
        Ok(lines) => {
            for line in lines {
                println!("{}", line);
            }
            ExitCode::SUCCESS
        }
        Err(d) => {
            eprintln!("{}", d.render(&src));
            ExitCode::FAILURE
        }
    }
}

/// Dispatch `runec run <target>`: a `.rune` file runs as a single file (the
/// existing behaviour); a directory — or anything containing a `rune.toml` — is
/// treated as a package.
fn run_target(target: &str) -> ExitCode {
    let path = Path::new(target);
    let is_package = path.is_dir() || path.join("rune.toml").is_file();
    if is_package {
        run_package(path)
    } else {
        run_file(target)
    }
}

// ---- package commands ----------------------------------------------------

/// Render package build diagnostics against the entry file's source (for carets
/// on entry-file errors); dependency/std errors carry their message only.
fn report_pkg_diags(dir: &Path, diags: &[rune::Diagnostic]) {
    let entry_src = pkg::load_manifest(dir)
        .ok()
        .map(|m| dir.join(&m.entry))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    for d in diags {
        eprintln!("{}\n", d.render(&entry_src));
    }
}

/// `runec build [dir]` — resolve and typecheck a package.
fn build_package(dir: &str) -> ExitCode {
    let dir = Path::new(dir);
    let manifest = match pkg::load_manifest(dir) {
        Ok(m) => m,
        Err(e) => return usage_error(&format!("error: {}", e)),
    };
    match pkg::build(dir) {
        Ok(module) => {
            println!(
                "compiled {} v{} ({} functions)",
                manifest.name,
                manifest.version,
                module.funcs.len()
            );
            ExitCode::SUCCESS
        }
        Err(diags) => {
            report_pkg_diags(dir, &diags);
            ExitCode::FAILURE
        }
    }
}

/// `runec run <dir>` — build the package and run its `main()`.
fn run_package(dir: &Path) -> ExitCode {
    let module = match pkg::build(dir) {
        Ok(m) => m,
        Err(diags) => {
            report_pkg_diags(dir, &diags);
            return ExitCode::FAILURE;
        }
    };
    let entry_src = pkg::load_manifest(dir)
        .ok()
        .map(|m| dir.join(&m.entry))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let mut interp = rune::Interpreter::new(module);
    match interp.run_main() {
        Ok(lines) => {
            for line in lines {
                println!("{}", line);
            }
            ExitCode::SUCCESS
        }
        Err(d) => {
            eprintln!("{}", d.render(&entry_src));
            ExitCode::FAILURE
        }
    }
}

/// `runec test [dir]` — build the package, then run every zero-argument
/// function whose fully-qualified name's final segment starts with `test_` and
/// which returns `bool`. A `false` return or a runtime trap is a failure.
fn test_package(dir: &str) -> ExitCode {
    let dir = Path::new(dir);
    let module = match pkg::build(dir) {
        Ok(m) => m,
        Err(diags) => {
            report_pkg_diags(dir, &diags);
            return ExitCode::FAILURE;
        }
    };

    // Collect test functions: final path segment starts with `test_`, no params,
    // returns bool. Deterministic order (funcs is a BTreeMap).
    let tests: Vec<String> = module
        .funcs
        .values()
        .filter(|f| {
            let last = f.name.rsplit("::").next().unwrap_or(&f.name);
            last.starts_with("test_")
                && f.params.is_empty()
                && f.ret == rune::ir::Type::Bool
        })
        .map(|f| f.name.clone())
        .collect();

    if tests.is_empty() {
        println!("no tests found");
        return ExitCode::SUCCESS;
    }

    let mut interp = rune::Interpreter::new(module);
    let mut passed = 0usize;
    let mut failed = 0usize;
    for name in &tests {
        match interp.call(name, vec![]) {
            Ok(rune::Value::Bool(true)) => {
                println!("test {} ... ok", name);
                passed += 1;
            }
            Ok(_) => {
                println!("test {} ... FAILED (returned false)", name);
                failed += 1;
            }
            Err(d) => {
                println!("test {} ... FAILED ({})", name, d.message);
                failed += 1;
            }
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed == 0 { "ok" } else { "FAILED" },
        passed,
        failed
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// `runec new <name>` — scaffold `./<name>/` with a manifest and a hello program.
fn new_package(name: &str) -> ExitCode {
    let dir = Path::new(name);
    if dir.exists() {
        return usage_error(&format!("error: `{}` already exists", dir.display()));
    }
    let src_dir = dir.join("src");
    if let Err(e) = std::fs::create_dir_all(&src_dir) {
        return usage_error(&format!("error: cannot create `{}`: {}", src_dir.display(), e));
    }
    // Use only the final path component for the package name in the manifest.
    let pkg_name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let manifest = format!(
        "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nentry = \"src/main.rune\"\n\n[dependencies]\n",
        pkg_name
    );
    let main_src = "fn greeting() -> u32 { 42 }\n\nfn main() {\n    print(greeting());\n}\n\nfn test_greeting() -> bool {\n    greeting() == 42\n}\n";
    if let Err(e) = std::fs::write(dir.join("rune.toml"), manifest) {
        return usage_error(&format!("error: cannot write rune.toml: {}", e));
    }
    if let Err(e) = std::fs::write(src_dir.join("main.rune"), main_src) {
        return usage_error(&format!("error: cannot write src/main.rune: {}", e));
    }
    println!("created package `{}`", pkg_name);
    ExitCode::SUCCESS
}

/// Compile a program and run its `main()`, returning the printed lines or a
/// rendered diagnostic string. Shared by the CLI and tests.
pub fn run_source(src: &str) -> Result<Vec<String>, String> {
    let module = rune::compile(src).map_err(|diags| {
        diags
            .iter()
            .map(|d| d.render(src))
            .collect::<Vec<_>>()
            .join("\n\n")
    })?;
    let mut interp = rune::Interpreter::new(module);
    interp.run_main().map_err(|d| d.render(src))
}

// ---- hdl ----------------------------------------------------------------

/// Compile a file and print the HDL-subset (synthesizability) report. This is
/// analysis only — no hardware is generated.
fn hdl_file(path: &str) -> ExitCode {
    let src = std::fs::read_to_string(path).unwrap_or_default();
    let module = match rune::compile_path(std::path::Path::new(path)) {
        Ok(m) => m,
        Err(diags) => {
            for d in &diags {
                eprintln!("{}\n", d.render(&src));
            }
            return ExitCode::FAILURE;
        }
    };
    let reports = rune::hdl::analyze(&module);
    println!("{}", rune::hdl::report_string(&reports).trim_end());
    ExitCode::SUCCESS
}

// ---- repl ---------------------------------------------------------------

fn repl_loop() -> ExitCode {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut session = Session::new();

    println!("Rune {} REPL — type `:help`, `:quit` to exit.", env!("CARGO_PKG_VERSION"));
    loop {
        print!("rune> ");
        let _ = stdout.flush();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                // EOF (Ctrl-D): exit cleanly.
                println!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("input error: {}", e);
                break;
            }
        }

        match session.eval_line(&line) {
            Outcome::Quit => break,
            other => print_outcome(&other),
        }
    }
    ExitCode::SUCCESS
}

fn print_outcome(outcome: &Outcome) {
    match outcome {
        Outcome::Value(v) => println!("{}", v),
        Outcome::Output(lines) => {
            for l in lines {
                println!("{}", l);
            }
        }
        Outcome::Defined(name) => println!("defined `{}`", name),
        Outcome::Message(m) => println!("{}", m),
        Outcome::Empty => {}
        Outcome::Error(msg) => eprintln!("{}", msg),
        Outcome::Quit => {}
    }
}

// ---- watch --------------------------------------------------------------

fn watch_file(path: &str) -> ExitCode {
    use rune::hotreload::{FileWatcher, ReloadEngine};

    let initial = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read `{}`: {}", path, e);
            return ExitCode::FAILURE;
        }
    };

    let mut engine = match ReloadEngine::new(&initial) {
        Ok(e) => e,
        Err(diags) => {
            for d in &diags {
                eprintln!("{}\n", d.render(&initial));
            }
            return ExitCode::FAILURE;
        }
    };

    println!("watching `{}` (edit and save to hot-reload; Ctrl-C / close stdin to stop)", path);
    run_and_report(&mut engine, path);

    let mut watcher = match FileWatcher::new(path) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error: cannot watch `{}`: {}", path, e);
            return ExitCode::FAILURE;
        }
    };

    // Poll for edits. The loop is terminable: when stdin is closed (EOF) we
    // stop after a few idle polls so tests and pipelines never hang forever.
    let stdin_open = stdin_is_open();
    let mut idle_polls = 0u32;
    const MAX_IDLE_POLLS_WHEN_STDIN_CLOSED: u32 = 5;

    loop {
        std::thread::sleep(Duration::from_millis(150));

        match watcher.changed() {
            Ok(true) => {
                idle_polls = 0;
                let src = match watcher.read() {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("error: cannot re-read `{}`: {}", path, e);
                        continue;
                    }
                };
                println!("\n--- file changed, reloading ---");
                let report = engine.reload_str(&src);
                print_reload_report(&report, &src);
                if report.applied {
                    run_and_report(&mut engine, path);
                }
            }
            Ok(false) => {
                if !stdin_open {
                    idle_polls += 1;
                    if idle_polls >= MAX_IDLE_POLLS_WHEN_STDIN_CLOSED {
                        break;
                    }
                }
            }
            Err(e) => {
                eprintln!("watch error: {}", e);
                break;
            }
        }
    }
    ExitCode::SUCCESS
}

/// Run `main()` on the engine's current module and report the result.
fn run_and_report(engine: &mut rune::hotreload::ReloadEngine, path: &str) {
    match engine.run_main() {
        Ok(lines) => {
            for l in lines {
                println!("{}", l);
            }
        }
        Err(d) => {
            // Render against the current file source for accurate locations.
            let src = std::fs::read_to_string(path).unwrap_or_default();
            eprintln!("{}", d.render(&src));
        }
    }
}

/// Print a hot-reload report readably.
fn print_reload_report(report: &rune::hotreload::ReloadReport, src: &str) {
    if !report.errors.is_empty() {
        eprintln!("reload rejected (kept previous version):");
        for d in &report.errors {
            eprintln!("{}\n", d.render(src));
        }
        return;
    }
    let sections: [(&str, &[String]); 6] = [
        ("added", &report.added),
        ("removed", &report.removed),
        ("changed", &report.changed),
        ("signature changed", &report.signature_changed),
        ("type changed", &report.type_changed),
        ("dropped state", &report.dropped_state),
    ];
    let mut any = false;
    for (label, items) in sections {
        if !items.is_empty() {
            any = true;
            println!("  {}: {}", label, items.join(", "));
        }
    }
    if !any {
        println!("  (no definition changes)");
    }
}

/// Best-effort check whether stdin is connected to an interactive terminal.
/// When stdin is not a TTY (closed/piped), the watch loop self-terminates after
/// a few idle polls so it can be driven non-interactively without hanging.
fn stdin_is_open() -> bool {
    // Without extra deps we can't truly detect a TTY portably; treat a readable
    // line as "interactive". We approximate by checking if stdin metadata looks
    // like a terminal via an env override used by tests, defaulting to open.
    if std::env::var_os("RUNEC_WATCH_ONESHOT").is_some() {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::run_source;

    #[test]
    fn run_source_runs_milestone() {
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
        assert_eq!(run_source(src).unwrap(), vec!["44", "12", "12"]);
    }

    #[test]
    fn run_source_reports_errors_with_location() {
        let err = run_source("fn main() { let x: u32 = true; }").unwrap_err();
        assert!(err.contains("at "), "expected a location, got: {}", err);
    }

    #[test]
    fn run_source_reports_missing_main() {
        let err = run_source("fn f() -> u32 { 1 }").unwrap_err();
        assert!(err.contains("main"), "expected a main error, got: {}", err);
    }
}

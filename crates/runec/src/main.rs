//! Minimal Rune CLI (spine). Phase-2 Agent B expands this into a polished REPL
//! and CLI; for now it can run a file end to end so the pipeline is usable.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") => match args.get(2) {
            Some(path) => run_file(path),
            None => {
                eprintln!("usage: runec run <file.rune>");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!(
                "Rune {}\nusage: runec run <file.rune>",
                env!("CARGO_PKG_VERSION")
            );
            ExitCode::FAILURE
        }
    }
}

fn run_file(path: &str) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read `{}`: {}", path, e);
            return ExitCode::FAILURE;
        }
    };
    let module = match rune::compile(&src) {
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

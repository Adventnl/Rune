//! The Rune REPL core, factored into a testable [`Session`].
//!
//! The session keeps two ordered lists: **item definitions** (`fn`/`struct`/
//! `enum`) and **session statements** (`let`/assignments/`expr;`). Evaluating an
//! expression synthesizes a throwaway program — all definitions plus an entry
//! function that replays every session statement and then `print`s the
//! expression — compiles it, and runs it. This gives clean, referentially
//! transparent semantics: variables persist because their `let`s are replayed in
//! order on every evaluation.
//!
//! Everything here is driven through [`Session::eval_line`], which takes a line
//! of input (no real stdin needed) and returns an [`Outcome`] describing what to
//! display. The interactive loop in `main.rs` is a thin shell over this.

use rune::Interpreter;

/// The name of the synthesized entry function used for evaluation/validation.
const ENTRY: &str = "__repl_entry";

/// What kind of top-level item a definition is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DefKind {
    Fn,
    Struct,
    Enum,
}

impl DefKind {
    fn keyword(self) -> &'static str {
        match self {
            DefKind::Fn => "fn",
            DefKind::Struct => "struct",
            DefKind::Enum => "enum",
        }
    }
}

/// A single stored item definition: its kind, name, and original source text.
#[derive(Clone, Debug)]
struct Def {
    kind: DefKind,
    name: String,
    text: String,
}

/// The result of feeding one line/block to the [`Session`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// An expression evaluated to a printable value (the rendered text).
    Value(String),
    /// One or more `print` lines were produced (e.g. by a statement run for
    /// effect, or an expression whose own evaluation printed).
    Output(Vec<String>),
    /// A definition (`fn`/`struct`/`enum`) was added or replaced; carries its
    /// name.
    Defined(String),
    /// The input did nothing observable (blank line, a command like `:reset`).
    Empty,
    /// Something went wrong; carries a human-readable (rendered) message.
    Error(String),
    /// A textual message to show the user (help, `:list` output, info).
    Message(String),
    /// The user asked to quit.
    Quit,
}

/// An interactive Rune session: accumulated definitions and statements.
#[derive(Default)]
pub struct Session {
    defs: Vec<Def>,
    stmts: Vec<String>,
}

impl Session {
    pub fn new() -> Session {
        Session::default()
    }

    /// Feed one line (or block) of REPL input; returns what to display.
    pub fn eval_line(&mut self, input: &str) -> Outcome {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Outcome::Empty;
        }
        if let Some(cmd) = trimmed.strip_prefix(':') {
            return self.run_command(cmd.trim());
        }
        match classify(trimmed) {
            Kind::Definition => self.add_definition(trimmed),
            Kind::Statement => self.add_statement(trimmed),
            Kind::Expression => self.eval_expression(trimmed),
        }
    }

    /// Current definition names, in definition order. (Part of the testable
    /// API; also handy for embedders inspecting a session.)
    #[allow(dead_code)]
    pub fn definition_names(&self) -> Vec<String> {
        self.defs.iter().map(|d| d.name.clone()).collect()
    }

    /// Clear all definitions and statements.
    pub fn reset(&mut self) {
        self.defs.clear();
        self.stmts.clear();
    }

    // ---- commands -------------------------------------------------------

    fn run_command(&mut self, cmd: &str) -> Outcome {
        let mut parts = cmd.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        match name {
            "help" | "h" | "?" => Outcome::Message(HELP.to_string()),
            "quit" | "q" | "exit" => Outcome::Quit,
            "list" | "l" => Outcome::Message(self.render_list()),
            "reset" => {
                self.reset();
                Outcome::Message("session reset".to_string())
            }
            "load" => {
                if arg.is_empty() {
                    return Outcome::Error("usage: :load <file>".to_string());
                }
                self.load_file(arg)
            }
            other => Outcome::Error(format!(
                "unknown command `:{}` (try `:help`)",
                other
            )),
        }
    }

    fn render_list(&self) -> String {
        let mut out = String::new();
        if self.defs.is_empty() && self.stmts.is_empty() {
            return "(session is empty)".to_string();
        }
        if !self.defs.is_empty() {
            out.push_str("definitions:\n");
            for d in &self.defs {
                out.push_str(&format!("  {} {}\n", d.kind.keyword(), d.name));
            }
        }
        if !self.stmts.is_empty() {
            out.push_str("statements:\n");
            for s in &self.stmts {
                out.push_str(&format!("  {}\n", s));
            }
        }
        out.truncate(out.trim_end().len());
        out
    }

    /// Read a file and feed each of its top-level items into the session.
    fn load_file(&mut self, path: &str) -> Outcome {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => return Outcome::Error(format!("cannot read `{}`: {}", path, e)),
        };
        // Validate the file as a whole first so we don't half-load on error.
        if let Err(msg) = compile_or_render(&src) {
            return Outcome::Error(msg);
        }
        let items = match split_items(&src) {
            Ok(items) => items,
            Err(msg) => return Outcome::Error(msg),
        };
        let mut loaded = Vec::new();
        for item in items {
            match self.add_definition(&item) {
                Outcome::Defined(name) => loaded.push(name),
                Outcome::Error(msg) => return Outcome::Error(msg),
                _ => {}
            }
        }
        if loaded.is_empty() {
            Outcome::Message(format!("loaded `{}` (no items)", path))
        } else {
            Outcome::Message(format!("loaded {}: {}", path, loaded.join(", ")))
        }
    }

    // ---- definitions ----------------------------------------------------

    fn add_definition(&mut self, text: &str) -> Outcome {
        let (kind, name) = match parse_def_header(text) {
            Some(h) => h,
            None => {
                return Outcome::Error(format!(
                    "could not determine the name of this {} definition",
                    classify(text).describe()
                ))
            }
        };

        // Tentatively install (replacing any same-named def) and validate the
        // whole session. Roll back on failure.
        let snapshot = self.defs.clone();
        self.defs.retain(|d| d.name != name);
        self.defs.push(Def {
            kind,
            name: name.clone(),
            text: text.to_string(),
        });

        match compile_or_render(&self.synthesize(None)) {
            Ok(()) => Outcome::Defined(name),
            Err(msg) => {
                self.defs = snapshot;
                Outcome::Error(msg)
            }
        }
    }

    // ---- statements -----------------------------------------------------

    fn add_statement(&mut self, text: &str) -> Outcome {
        let stmt = text.to_string();
        self.stmts.push(stmt);

        match compile_or_render(&self.synthesize(None)) {
            Ok(()) => Outcome::Empty,
            Err(msg) => {
                self.stmts.pop();
                Outcome::Error(msg)
            }
        }
    }

    // ---- expressions ----------------------------------------------------

    fn eval_expression(&mut self, expr: &str) -> Outcome {
        // First try `print(<expr>)` so we display the value.
        let printed = self.synthesize(Some(&format!("print({});", expr)));
        match self.run_entry(&printed) {
            Ok(lines) => return as_value_outcome(lines),
            Err(EvalError::Compile(msg)) => {
                // If `print` failed *specifically* because the expression is
                // unit-typed, retry running it for effect (no value to show).
                if msg.contains("cannot print a value of type `()`") {
                    let effect = self.synthesize(Some(&format!("{};", expr)));
                    return match self.run_entry(&effect) {
                        Ok(lines) => {
                            if lines.is_empty() {
                                Outcome::Empty
                            } else {
                                Outcome::Output(lines)
                            }
                        }
                        Err(e) => Outcome::Error(e.into_message()),
                    };
                }
                Outcome::Error(msg)
            }
            Err(e) => Outcome::Error(e.into_message()),
        }
    }

    /// Compile and run the synthesized entry function, returning its output.
    fn run_entry(&self, src: &str) -> Result<Vec<String>, EvalError> {
        let module = rune::compile(src).map_err(|diags| {
            EvalError::Compile(render_diags(&diags, src))
        })?;
        let mut interp = Interpreter::new(module);
        match interp.call(ENTRY, vec![]) {
            Ok(_) => Ok(interp.take_output()),
            Err(d) => Err(EvalError::Runtime(d.render(src))),
        }
    }

    /// Build a full program text: every definition, then an entry function
    /// replaying all session statements (and an optional trailing line such as
    /// `print(E);`).
    fn synthesize(&self, trailer: Option<&str>) -> String {
        let mut out = String::new();
        for d in &self.defs {
            out.push_str(&d.text);
            out.push('\n');
        }
        out.push_str("fn ");
        out.push_str(ENTRY);
        out.push_str("() {\n");
        for s in &self.stmts {
            out.push_str("    ");
            out.push_str(s);
            out.push('\n');
        }
        if let Some(t) = trailer {
            out.push_str("    ");
            out.push_str(t);
            out.push('\n');
        }
        out.push_str("}\n");
        out
    }
}

/// Map captured output lines to an outcome for an evaluated expression.
fn as_value_outcome(mut lines: Vec<String>) -> Outcome {
    match lines.len() {
        0 => Outcome::Empty,
        1 => Outcome::Value(lines.pop().unwrap()),
        _ => Outcome::Output(lines),
    }
}

/// An error encountered while evaluating an expression.
enum EvalError {
    Compile(String),
    Runtime(String),
}

impl EvalError {
    fn into_message(self) -> String {
        match self {
            EvalError::Compile(m) | EvalError::Runtime(m) => m,
        }
    }
}

/// How an input line is classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Definition,
    Statement,
    Expression,
}

impl Kind {
    fn describe(self) -> &'static str {
        match self {
            Kind::Definition => "item",
            Kind::Statement => "statement",
            Kind::Expression => "expression",
        }
    }
}

/// Classify a (trimmed, non-command) input line.
fn classify(input: &str) -> Kind {
    if starts_with_keyword(input, "fn")
        || starts_with_keyword(input, "struct")
        || starts_with_keyword(input, "enum")
    {
        Kind::Definition
    } else if starts_with_keyword(input, "let")
        || starts_with_keyword(input, "while")
        || starts_with_keyword(input, "for")
        || starts_with_keyword(input, "return")
        || input.ends_with(';')
    {
        Kind::Statement
    } else {
        Kind::Expression
    }
}

/// True if `input` begins with `kw` as a whole word (followed by a non-ident
/// char or end of string).
fn starts_with_keyword(input: &str, kw: &str) -> bool {
    match input.strip_prefix(kw) {
        Some(rest) => rest
            .chars()
            .next()
            .map(|c| !is_ident_char(c))
            .unwrap_or(true),
        None => false,
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Extract `(kind, name)` from a definition's header, e.g. `fn dbl(x: u32)` →
/// `(Fn, "dbl")`, `struct P {` → `(Struct, "P")`.
fn parse_def_header(text: &str) -> Option<(DefKind, String)> {
    let text = text.trim_start();
    let (kind, rest) = if let Some(r) = text.strip_prefix("fn") {
        (DefKind::Fn, r)
    } else if let Some(r) = text.strip_prefix("struct") {
        (DefKind::Struct, r)
    } else if let Some(r) = text.strip_prefix("enum") {
        (DefKind::Enum, r)
    } else {
        return None;
    };
    let rest = rest.trim_start();
    let name: String = rest.chars().take_while(|c| is_ident_char(*c)).collect();
    if name.is_empty() {
        None
    } else {
        Some((kind, name))
    }
}

/// Split a source file into its top-level item texts by tracking brace depth.
/// Each item runs from a top-level `fn`/`struct`/`enum` keyword up to the
/// matching close of its outermost `{...}` block (or, for a brace-less item, the
/// next top-level item or end of input).
fn split_items(src: &str) -> Result<Vec<String>, String> {
    let bytes = src.as_bytes();
    let mut items = Vec::new();
    let mut depth: i32 = 0;
    let mut item_start: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        // Skip line comments so braces inside them don't confuse depth.
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = item_start.take() {
                        items.push(src[start..=i].trim().to_string());
                    }
                }
                if depth < 0 {
                    return Err("unbalanced `}` while loading file".to_string());
                }
            }
            _ => {}
        }
        if depth == 0 && item_start.is_none() && (c == 'f' || c == 's' || c == 'e') {
            let tail = &src[i..];
            if starts_with_keyword(tail, "fn")
                || starts_with_keyword(tail, "struct")
                || starts_with_keyword(tail, "enum")
            {
                item_start = Some(i);
            }
        }
        i += 1;
    }
    if depth != 0 {
        return Err("unbalanced braces while loading file".to_string());
    }
    Ok(items)
}

/// Compile a program for validation only, rendering any diagnostics.
fn compile_or_render(src: &str) -> Result<(), String> {
    rune::compile(src)
        .map(|_| ())
        .map_err(|diags| render_diags(&diags, src))
}

/// Render a batch of diagnostics against the source they came from.
fn render_diags(diags: &[rune::Diagnostic], src: &str) -> String {
    diags
        .iter()
        .map(|d| d.render(src))
        .collect::<Vec<_>>()
        .join("\n\n")
}

const HELP: &str = "\
Rune REPL commands:
  :help            show this help
  :list            list current definitions and statements
  :load <file>     load items from a .rune file into the session
  :reset           clear the session
  :quit            exit the REPL (also Ctrl-D)

Enter `fn`/`struct`/`enum` to define items, `let`/assignments/`stmt;` to add
statements, or any expression to evaluate and print it.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_literal_expression() {
        let mut s = Session::new();
        assert_eq!(s.eval_line("1 + 2 * 3"), Outcome::Value("7".to_string()));
    }

    #[test]
    fn define_then_call_function() {
        let mut s = Session::new();
        assert_eq!(
            s.eval_line("fn dbl(x: u32) -> u32 { x * 2 }"),
            Outcome::Defined("dbl".to_string())
        );
        assert_eq!(s.eval_line("dbl(21)"), Outcome::Value("42".to_string()));
    }

    #[test]
    fn persistent_statements() {
        let mut s = Session::new();
        assert_eq!(s.eval_line("let x: u32 = 10;"), Outcome::Empty);
        assert_eq!(s.eval_line("let y: u32 = 32;"), Outcome::Empty);
        assert_eq!(s.eval_line("x + y"), Outcome::Value("42".to_string()));
    }

    #[test]
    fn mutable_statement_persists() {
        let mut s = Session::new();
        assert_eq!(s.eval_line("let mut n: u32 = 1;"), Outcome::Empty);
        assert_eq!(s.eval_line("n = n + 41;"), Outcome::Empty);
        assert_eq!(s.eval_line("n"), Outcome::Value("42".to_string()));
    }

    #[test]
    fn define_enum_and_use_it() {
        let mut s = Session::new();
        assert_eq!(
            s.eval_line("enum Shape { Circle(u32), Rect(u32, u32) }"),
            Outcome::Defined("Shape".to_string())
        );
        assert_eq!(
            s.eval_line("fn area(sh: Shape) -> u32 { match sh { Circle(r) => 3 * r * r, Rect(w, h) => w * h } }"),
            Outcome::Defined("area".to_string())
        );
        assert_eq!(
            s.eval_line("area(Rect(3, 4))"),
            Outcome::Value("12".to_string())
        );
        // The enum value itself prints with its variant name.
        assert_eq!(
            s.eval_line("Rect(3, 4)"),
            Outcome::Value("Rect(3, 4)".to_string())
        );
    }

    #[test]
    fn define_struct_and_use_it() {
        let mut s = Session::new();
        assert_eq!(
            s.eval_line("struct P { x: u32, y: u32 }"),
            Outcome::Defined("P".to_string())
        );
        assert_eq!(s.eval_line("let p = P { x: 11, y: 2 };"), Outcome::Empty);
        assert_eq!(s.eval_line("p.x"), Outcome::Value("11".to_string()));
    }

    #[test]
    fn type_error_is_reported_with_location_and_no_corruption() {
        let mut s = Session::new();
        let out = s.eval_line("true + 1");
        match out {
            Outcome::Error(msg) => {
                assert!(msg.contains("at "), "expected a location in: {}", msg);
            }
            other => panic!("expected Error, got {:?}", other),
        }
        // Session not corrupted: a subsequent valid line still works.
        assert_eq!(s.eval_line("2 + 2"), Outcome::Value("4".to_string()));
    }

    #[test]
    fn bad_statement_rolls_back() {
        let mut s = Session::new();
        assert_eq!(s.eval_line("let x: u32 = 1;"), Outcome::Empty);
        // References an unknown variable -> error, must roll back.
        assert!(matches!(
            s.eval_line("let y: u32 = nope;"),
            Outcome::Error(_)
        ));
        // x is still usable; y was never added.
        assert_eq!(s.eval_line("x + 1"), Outcome::Value("2".to_string()));
    }

    #[test]
    fn redefinition_replaces_previous() {
        let mut s = Session::new();
        assert_eq!(
            s.eval_line("fn f() -> u32 { 1 }"),
            Outcome::Defined("f".to_string())
        );
        assert_eq!(s.eval_line("f()"), Outcome::Value("1".to_string()));
        // Redefining the same name must replace, not error as a duplicate.
        assert_eq!(
            s.eval_line("fn f() -> u32 { 2 }"),
            Outcome::Defined("f".to_string())
        );
        assert_eq!(s.eval_line("f()"), Outcome::Value("2".to_string()));
        assert_eq!(s.definition_names(), vec!["f".to_string()]);
    }

    #[test]
    fn list_and_reset_commands() {
        let mut s = Session::new();
        s.eval_line("fn a() -> u32 { 1 }");
        s.eval_line("struct B { x: u32 }");
        assert_eq!(s.definition_names(), vec!["a".to_string(), "B".to_string()]);
        let listed = match s.eval_line(":list") {
            Outcome::Message(m) => m,
            other => panic!("expected Message, got {:?}", other),
        };
        assert!(listed.contains("a"));
        assert!(listed.contains("B"));

        assert!(matches!(s.eval_line(":reset"), Outcome::Message(_)));
        assert!(s.definition_names().is_empty());
    }

    #[test]
    fn unknown_command_is_friendly_error() {
        let mut s = Session::new();
        match s.eval_line(":frobnicate") {
            Outcome::Error(msg) => assert!(msg.contains("unknown command")),
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn quit_command() {
        let mut s = Session::new();
        assert_eq!(s.eval_line(":quit"), Outcome::Quit);
        assert_eq!(s.eval_line(":q"), Outcome::Quit);
    }

    #[test]
    fn empty_line_is_empty() {
        let mut s = Session::new();
        assert_eq!(s.eval_line("   "), Outcome::Empty);
    }

    #[test]
    fn expression_that_prints_shows_output() {
        let mut s = Session::new();
        // A unit-typed expression with a side effect (print) runs for effect.
        assert_eq!(
            s.eval_line("print(7)"),
            Outcome::Output(vec!["7".to_string()])
        );
    }

    #[test]
    fn split_items_separates_top_level() {
        let src = "fn a() -> u32 { 1 }\nstruct B { x: u32 }\nenum E { X, Y }";
        let items = split_items(src).unwrap();
        assert_eq!(items.len(), 3);
        assert!(items[0].starts_with("fn a"));
        assert!(items[1].starts_with("struct B"));
        assert!(items[2].starts_with("enum E"));
    }
}

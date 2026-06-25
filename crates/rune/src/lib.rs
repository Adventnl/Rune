//! # Rune
//!
//! Rune is a small, statically-typed systems language (C++ in spirit: value
//! semantics, explicit types, no GC) architected around **one typed core IR**
//! with two intended targets:
//!
//! 1. a live, hot-reloadable tree-walking interpreter (built here), and
//! 2. a synthesizable subset that a future backend lowers to Verilog/HDL
//!    (design-for only — analysed by [`hdl`], never codegen'd here).
//!
//! ## Pipeline
//!
//! ```text
//! source → lexer → parser → AST → TYPED CORE IR → interpreter over IR
//! ```
//!
//! The [`ir`] module is the frozen contract shared by every stage. See
//! `docs/ir.md` for the design invariants.

pub mod ast;
pub mod diagnostic;
pub mod ir;
pub mod span;

pub mod lexer;
pub mod loader;
pub mod parser;
pub mod typeck;
pub mod interp;

pub mod hdl;
pub mod hotreload;
pub mod verilog;

pub use diagnostic::{Diagnostic, Stage};
pub use interp::{Interpreter, Value};
pub use span::Span;

/// Compile a source string all the way to a typed IR [`ir::Module`].
///
/// This is the canonical front-end entry point: lex → parse → typecheck. It
/// returns the first batch of diagnostics on failure.
pub fn compile(src: &str) -> Result<ir::Module, Vec<Diagnostic>> {
    let tokens = lexer::lex(src).map_err(|d| vec![d])?;
    let program = parser::parse(&tokens).map_err(|d| vec![d])?;
    typeck::check(&program)
}

pub use loader::{compile_path, load_program};

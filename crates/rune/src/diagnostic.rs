//! Structured, located diagnostics shared across pipeline stages.
//!
//! Parse errors and type errors are *values*, never panics. Each diagnostic
//! carries a message, a primary [`Span`], and a stage label so the CLI can
//! render readable errors with line/column information.

use crate::span::{line_col, Span};
use std::fmt;

/// The pipeline stage that produced a diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Lex,
    Parse,
    Type,
    Runtime,
    Reload,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Stage::Lex => "lex error",
            Stage::Parse => "parse error",
            Stage::Type => "type error",
            Stage::Runtime => "runtime error",
            Stage::Reload => "reload error",
        };
        f.write_str(s)
    }
}

/// A structured diagnostic with a source location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub stage: Stage,
    pub message: String,
    pub span: Span,
}

impl Diagnostic {
    pub fn new(stage: Stage, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            stage,
            message: message.into(),
            span,
        }
    }

    /// Render the diagnostic against the original source, including a caret
    /// line pointing at the offending span.
    pub fn render(&self, src: &str) -> String {
        let (line, col) = line_col(src, self.span.start);
        let mut out = format!("{}: {} (at {}:{})\n", self.stage, self.message, line, col);
        if let Some(text) = src.lines().nth(line - 1) {
            out.push_str(&format!("  {:>4} | {}\n", line, text));
            let caret_pad = col.saturating_sub(1);
            let caret_len = (self.span.end.saturating_sub(self.span.start)).max(1);
            out.push_str(&format!(
                "       | {}{}",
                " ".repeat(caret_pad),
                "^".repeat(caret_len)
            ));
        }
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.stage, self.message)
    }
}

impl std::error::Error for Diagnostic {}

//! # Lexer
//!
//! Converts a source string into a flat [`Vec`] of [`Token`]s. The lexer
//! recognises all literals, identifiers, keywords, and operators of Rune v1.
//! Errors (e.g. an unterminated string or stray character) are returned as a
//! structured [`Diagnostic`], never a panic.

use crate::diagnostic::{Diagnostic, Stage};
use crate::span::Span;

/// A lexical token kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tok {
    // Literals & identifiers
    Int(i128),
    Ident(String),

    // Keywords
    Fn,
    Struct,
    Enum,
    Let,
    Mut,
    If,
    Else,
    While,
    For,
    In,
    Match,
    Return,
    Mod,
    Use,
    True,
    False,
    Bool,
    Bit,
    /// One of i8/i16/i32/i64/u8/u16/u32/u64, carrying (signed, width).
    IntTy(bool, u32),

    // Punctuation / operators
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Colon,
    ColonColon, // ::
    Dot,
    DotDot,
    DotDotEq, // ..=
    Arrow,    // ->
    FatArrow, // =>
    Eq,       // =
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    AndAnd,
    OrOr,
    Amp,
    Pipe,
    Caret,
    Shl,
    Shr,
    Bang,

    /// End of input.
    Eof,
}

/// A token with its source span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

/// Lex `src` into tokens, terminated by a single [`Tok::Eof`].
pub fn lex(src: &str) -> Result<Vec<Token>, Diagnostic> {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();

    while i < bytes.len() {
        let c = bytes[i];

        // Whitespace
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Line comments `// ...`
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comments `/* ... */` (non-nested)
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            let mut closed = false;
            while i + 1 < bytes.len() {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(Diagnostic::new(
                    Stage::Lex,
                    "unterminated block comment",
                    Span::new(start, src.len()),
                ));
            }
            continue;
        }

        let start = i;

        // Numbers (decimal and hex)
        if c.is_ascii_digit() {
            let (value, end) = lex_number(src, bytes, i)?;
            out.push(Token {
                tok: Tok::Int(value),
                span: Span::new(start, end),
            });
            i = end;
            continue;
        }

        // Identifiers & keywords
        if c == b'_' || c.is_ascii_alphabetic() {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b'_' || bytes[j].is_ascii_alphanumeric()) {
                j += 1;
            }
            let word = &src[i..j];
            out.push(Token {
                tok: keyword_or_ident(word),
                span: Span::new(start, j),
            });
            i = j;
            continue;
        }

        // Operators & punctuation (longest-match first)
        let (tok, len) = match_operator(&bytes[i..]).ok_or_else(|| {
            Diagnostic::new(
                Stage::Lex,
                format!("unexpected character '{}'", c as char),
                Span::new(start, start + 1),
            )
        })?;
        out.push(Token {
            tok,
            span: Span::new(start, start + len),
        });
        i += len;
    }

    out.push(Token {
        tok: Tok::Eof,
        span: Span::new(src.len(), src.len()),
    });
    Ok(out)
}

fn lex_number(src: &str, bytes: &[u8], i: usize) -> Result<(i128, usize), Diagnostic> {
    // Hex literal
    if bytes[i] == b'0' && i + 1 < bytes.len() && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X') {
        let mut j = i + 2;
        let digit_start = j;
        while j < bytes.len() && (bytes[j] == b'_' || bytes[j].is_ascii_hexdigit()) {
            j += 1;
        }
        if j == digit_start {
            return Err(Diagnostic::new(
                Stage::Lex,
                "hex literal has no digits",
                Span::new(i, j),
            ));
        }
        let raw: String = src[digit_start..j].chars().filter(|&c| c != '_').collect();
        let value = i128::from_str_radix(&raw, 16).map_err(|_| {
            Diagnostic::new(Stage::Lex, "integer literal out of range", Span::new(i, j))
        })?;
        return Ok((value, j));
    }

    // Decimal literal
    let mut j = i;
    while j < bytes.len() && (bytes[j] == b'_' || bytes[j].is_ascii_digit()) {
        j += 1;
    }
    let raw: String = src[i..j].chars().filter(|&c| c != '_').collect();
    let value = raw.parse::<i128>().map_err(|_| {
        Diagnostic::new(Stage::Lex, "integer literal out of range", Span::new(i, j))
    })?;
    Ok((value, j))
}

fn keyword_or_ident(word: &str) -> Tok {
    match word {
        "fn" => Tok::Fn,
        "struct" => Tok::Struct,
        "enum" => Tok::Enum,
        "let" => Tok::Let,
        "mut" => Tok::Mut,
        "if" => Tok::If,
        "else" => Tok::Else,
        "while" => Tok::While,
        "for" => Tok::For,
        "in" => Tok::In,
        "match" => Tok::Match,
        "return" => Tok::Return,
        "mod" => Tok::Mod,
        "use" => Tok::Use,
        "true" => Tok::True,
        "false" => Tok::False,
        "bool" => Tok::Bool,
        "bit" => Tok::Bit,
        "i8" => Tok::IntTy(true, 8),
        "i16" => Tok::IntTy(true, 16),
        "i32" => Tok::IntTy(true, 32),
        "i64" => Tok::IntTy(true, 64),
        "u8" => Tok::IntTy(false, 8),
        "u16" => Tok::IntTy(false, 16),
        "u32" => Tok::IntTy(false, 32),
        "u64" => Tok::IntTy(false, 64),
        _ => Tok::Ident(word.to_string()),
    }
}

/// Match a single operator/punctuation token at the start of `b`, returning the
/// token and how many bytes it consumed. Longest match wins.
fn match_operator(b: &[u8]) -> Option<(Tok, usize)> {
    // Three-character operators first.
    if b.len() >= 3 && &b[..3] == b"..=" {
        return Some((Tok::DotDotEq, 3));
    }
    // Two-character operators next.
    if b.len() >= 2 {
        let two = &b[..2];
        let t = match two {
            b"->" => Some(Tok::Arrow),
            b"=>" => Some(Tok::FatArrow),
            b"==" => Some(Tok::EqEq),
            b"!=" => Some(Tok::Ne),
            b"<=" => Some(Tok::Le),
            b">=" => Some(Tok::Ge),
            b"&&" => Some(Tok::AndAnd),
            b"||" => Some(Tok::OrOr),
            b"<<" => Some(Tok::Shl),
            b">>" => Some(Tok::Shr),
            b".." => Some(Tok::DotDot),
            b"::" => Some(Tok::ColonColon),
            _ => None,
        };
        if let Some(t) = t {
            return Some((t, 2));
        }
    }

    let t = match b[0] {
        b'(' => Tok::LParen,
        b')' => Tok::RParen,
        b'{' => Tok::LBrace,
        b'}' => Tok::RBrace,
        b'[' => Tok::LBracket,
        b']' => Tok::RBracket,
        b',' => Tok::Comma,
        b';' => Tok::Semi,
        b':' => Tok::Colon,
        b'.' => Tok::Dot,
        b'=' => Tok::Eq,
        b'<' => Tok::Lt,
        b'>' => Tok::Gt,
        b'+' => Tok::Plus,
        b'-' => Tok::Minus,
        b'*' => Tok::Star,
        b'/' => Tok::Slash,
        b'%' => Tok::Percent,
        b'&' => Tok::Amp,
        b'|' => Tok::Pipe,
        b'^' => Tok::Caret,
        b'!' => Tok::Bang,
        _ => return None,
    };
    Some((t, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn lexes_keywords_and_idents() {
        assert_eq!(
            kinds("fn add foo_bar"),
            vec![
                Tok::Fn,
                Tok::Ident("add".into()),
                Tok::Ident("foo_bar".into()),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn lexes_int_types_and_bit() {
        assert_eq!(
            kinds("bit i8 u64 bool"),
            vec![
                Tok::Bit,
                Tok::IntTy(true, 8),
                Tok::IntTy(false, 64),
                Tok::Bool,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn lexes_numbers_decimal_hex_underscore() {
        assert_eq!(
            kinds("0 42 1_000 0xFF 0x1_0"),
            vec![
                Tok::Int(0),
                Tok::Int(42),
                Tok::Int(1000),
                Tok::Int(255),
                Tok::Int(16),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn lexes_operators() {
        assert_eq!(
            kinds("-> => == != <= >= && || << >> .. + - * / % & | ^ ! < > ="),
            vec![
                Tok::Arrow,
                Tok::FatArrow,
                Tok::EqEq,
                Tok::Ne,
                Tok::Le,
                Tok::Ge,
                Tok::AndAnd,
                Tok::OrOr,
                Tok::Shl,
                Tok::Shr,
                Tok::DotDot,
                Tok::Plus,
                Tok::Minus,
                Tok::Star,
                Tok::Slash,
                Tok::Percent,
                Tok::Amp,
                Tok::Pipe,
                Tok::Caret,
                Tok::Bang,
                Tok::Lt,
                Tok::Gt,
                Tok::Eq,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn skips_comments() {
        assert_eq!(
            kinds("fn // line\n add /* block */ x"),
            vec![Tok::Fn, Tok::Ident("add".into()), Tok::Ident("x".into()), Tok::Eof]
        );
    }

    #[test]
    fn milestone_sample_lexes() {
        let src = "fn add8(a: bit<8>, b: bit<8>) -> bit<8> { a + b }";
        // Spot-check a couple of interesting tokens and that it ends in Eof.
        let ts = kinds(src);
        assert_eq!(ts[0], Tok::Fn);
        assert!(ts.contains(&Tok::Bit));
        assert!(ts.contains(&Tok::Arrow));
        assert_eq!(*ts.last().unwrap(), Tok::Eof);
    }

    #[test]
    fn errors_on_stray_char() {
        let err = lex("fn @").unwrap_err();
        assert_eq!(err.stage, Stage::Lex);
    }
}

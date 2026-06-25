//! # Parser
//!
//! A hand-written recursive-descent parser with Pratt-style operator-precedence
//! expression parsing. It turns the token stream into an [`ast::Program`].
//!
//! Parse errors are structured [`Diagnostic`]s with source spans — the parser
//! never panics on malformed input.

use crate::ast::*;
use crate::diagnostic::{Diagnostic, Stage};
use crate::lexer::{Tok, Token};
use crate::span::Span;

type PResult<T> = Result<T, Diagnostic>;

/// Parse a full program from a token stream (as produced by [`crate::lexer::lex`]).
pub fn parse(tokens: &[Token]) -> PResult<Program> {
    let mut p = Parser { tokens, pos: 0 };
    p.program()
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    // ---- token cursor helpers -------------------------------------------

    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }

    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at(&self, t: &Tok) -> bool {
        self.peek() == t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.at(t) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok, what: &str) -> PResult<Token> {
        if self.at(t) {
            Ok(self.bump())
        } else {
            Err(self.err(format!("expected {}, found {}", what, describe(self.peek()))))
        }
    }

    fn err(&self, msg: impl Into<String>) -> Diagnostic {
        Diagnostic::new(Stage::Parse, msg, self.span())
    }

    fn ident(&mut self) -> PResult<(String, Span)> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.bump();
                Ok((name, span))
            }
            other => Err(self.err(format!("expected identifier, found {}", describe(&other)))),
        }
    }

    // ---- items ----------------------------------------------------------

    fn program(&mut self) -> PResult<Program> {
        let mut items = Vec::new();
        while !self.at(&Tok::Eof) {
            items.push(self.item()?);
        }
        Ok(Program { items })
    }

    fn item(&mut self) -> PResult<Item> {
        match self.peek() {
            Tok::Fn => self.func().map(Item::Func),
            Tok::Struct => self.struct_def().map(Item::Struct),
            Tok::Enum => self.enum_def().map(Item::Enum),
            Tok::Mod => self.mod_def().map(Item::Mod),
            Tok::Use => self.use_decl().map(Item::Use),
            other => Err(self.err(format!(
                "expected an item (fn, struct, enum, mod, or use), found {}",
                describe(other)
            ))),
        }
    }

    fn mod_def(&mut self) -> PResult<ModDef> {
        let start = self.span();
        self.expect(&Tok::Mod, "`mod`")?;
        let (name, _) = self.ident()?;
        if self.eat(&Tok::Semi) {
            // File module: `mod name;` resolved to a sibling file by the loader.
            return Ok(ModDef {
                name,
                items: None,
                span: start.merge(self.prev_span()),
            });
        }
        self.expect(&Tok::LBrace, "`{` or `;` after module name")?;
        let mut items = Vec::new();
        while !self.at(&Tok::RBrace) && !self.at(&Tok::Eof) {
            items.push(self.item()?);
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(ModDef {
            name,
            items: Some(items),
            span: start.merge(self.prev_span()),
        })
    }

    fn use_decl(&mut self) -> PResult<UseDecl> {
        let start = self.span();
        self.expect(&Tok::Use, "`use`")?;
        let (first, _) = self.ident()?;
        let mut path = vec![first];
        let mut glob = false;
        while self.eat(&Tok::ColonColon) {
            if self.eat(&Tok::Star) {
                glob = true;
                break;
            }
            let (seg, _) = self.ident()?;
            path.push(seg);
        }
        self.expect(&Tok::Semi, "`;` after use")?;
        Ok(UseDecl {
            path,
            glob,
            span: start.merge(self.prev_span()),
        })
    }

    /// Parse a `::`-separated path starting at the current identifier:
    /// `a` → `["a"]`, `a::b::c` → `["a","b","c"]`.
    fn name_path(&mut self) -> PResult<Path> {
        let (first, _) = self.ident()?;
        let mut segs = vec![first];
        while self.eat(&Tok::ColonColon) {
            let (seg, _) = self.ident()?;
            segs.push(seg);
        }
        Ok(segs)
    }

    fn func(&mut self) -> PResult<Func> {
        let start = self.span();
        self.expect(&Tok::Fn, "`fn`")?;
        let (name, _) = self.ident()?;
        self.expect(&Tok::LParen, "`(`")?;
        let mut params = Vec::new();
        while !self.at(&Tok::RParen) {
            let pstart = self.span();
            let (pname, _) = self.ident()?;
            self.expect(&Tok::Colon, "`:`")?;
            let ty = self.type_expr()?;
            params.push(Param {
                name: pname,
                ty,
                span: pstart.merge(self.prev_span()),
            });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, "`)`")?;
        let ret = if self.eat(&Tok::Arrow) {
            Some(self.type_expr()?)
        } else {
            None
        };
        let body = self.block()?;
        Ok(Func {
            name,
            params,
            ret,
            span: start.merge(self.prev_span()),
            body,
        })
    }

    fn struct_def(&mut self) -> PResult<StructDef> {
        let start = self.span();
        self.expect(&Tok::Struct, "`struct`")?;
        let (name, _) = self.ident()?;
        self.expect(&Tok::LBrace, "`{`")?;
        let mut fields = Vec::new();
        while !self.at(&Tok::RBrace) {
            let fstart = self.span();
            let (fname, _) = self.ident()?;
            self.expect(&Tok::Colon, "`:`")?;
            let ty = self.type_expr()?;
            fields.push(FieldDef {
                name: fname,
                ty,
                span: fstart.merge(self.prev_span()),
            });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(StructDef {
            name,
            fields,
            span: start.merge(self.prev_span()),
        })
    }

    fn enum_def(&mut self) -> PResult<EnumDef> {
        let start = self.span();
        self.expect(&Tok::Enum, "`enum`")?;
        let (name, _) = self.ident()?;
        self.expect(&Tok::LBrace, "`{`")?;
        let mut variants = Vec::new();
        while !self.at(&Tok::RBrace) {
            let vstart = self.span();
            let (vname, _) = self.ident()?;
            let mut field_tys = Vec::new();
            if self.eat(&Tok::LParen) {
                while !self.at(&Tok::RParen) {
                    field_tys.push(self.type_expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)`")?;
            }
            variants.push(VariantDef {
                name: vname,
                fields: field_tys,
                span: vstart.merge(self.prev_span()),
            });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(EnumDef {
            name,
            variants,
            span: start.merge(self.prev_span()),
        })
    }

    // ---- types ----------------------------------------------------------

    fn type_expr(&mut self) -> PResult<TypeExpr> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Bool => {
                self.bump();
                Ok(TypeExpr::Bool)
            }
            Tok::IntTy(signed, width) => {
                self.bump();
                Ok(TypeExpr::Int { signed, width })
            }
            Tok::Bit => {
                self.bump();
                self.expect(&Tok::Lt, "`<`")?;
                let width = self.int_literal("bit width")?;
                self.expect(&Tok::Gt, "`>`")?;
                if width <= 0 || width > 128 {
                    return Err(Diagnostic::new(
                        Stage::Parse,
                        "bit<N> width must be in 1..=128",
                        span.merge(self.prev_span()),
                    ));
                }
                Ok(TypeExpr::Bit {
                    width: width as u32,
                })
            }
            Tok::LBracket => {
                self.bump();
                let elem = self.type_expr()?;
                self.expect(&Tok::Semi, "`;`")?;
                let len = self.int_literal("array length")?;
                self.expect(&Tok::RBracket, "`]`")?;
                if len < 0 {
                    return Err(Diagnostic::new(
                        Stage::Parse,
                        "array length must be non-negative",
                        span.merge(self.prev_span()),
                    ));
                }
                Ok(TypeExpr::Array {
                    elem: Box::new(elem),
                    len: len as u64,
                })
            }
            Tok::LParen => {
                self.bump();
                if self.eat(&Tok::RParen) {
                    return Ok(TypeExpr::Unit);
                }
                let mut elems = Vec::new();
                let mut trailing_comma = false;
                while !self.at(&Tok::RParen) {
                    elems.push(self.type_expr()?);
                    if self.eat(&Tok::Comma) {
                        trailing_comma = true;
                    } else {
                        trailing_comma = false;
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)` in type")?;
                if elems.len() == 1 && !trailing_comma {
                    Ok(elems.into_iter().next().unwrap())
                } else {
                    Ok(TypeExpr::Tuple(elems))
                }
            }
            Tok::Ident(_) => {
                let segs = self.name_path()?;
                Ok(TypeExpr::Named(segs))
            }
            other => Err(self.err(format!("expected a type, found {}", describe(&other)))),
        }
    }

    fn int_literal(&mut self, what: &str) -> PResult<i128> {
        match self.peek().clone() {
            Tok::Int(v) => {
                self.bump();
                Ok(v)
            }
            other => Err(self.err(format!(
                "expected {} (integer literal), found {}",
                what,
                describe(&other)
            ))),
        }
    }

    // ---- blocks & statements -------------------------------------------

    fn block(&mut self) -> PResult<Block> {
        let start = self.span();
        self.expect(&Tok::LBrace, "`{`")?;
        let mut stmts = Vec::new();
        let mut tail = None;

        while !self.at(&Tok::RBrace) && !self.at(&Tok::Eof) {
            // Statement keywords.
            match self.peek() {
                Tok::Let => {
                    stmts.extend(self.let_stmt()?);
                    continue;
                }
                Tok::While => {
                    stmts.push(self.while_stmt()?);
                    continue;
                }
                Tok::For => {
                    stmts.push(self.for_stmt()?);
                    continue;
                }
                Tok::Return => {
                    stmts.push(self.return_stmt()?);
                    continue;
                }
                _ => {}
            }

            // Otherwise: an expression. It may be a block-like expression used
            // as a statement (if/match/{...}), an assignment, an
            // expression-statement (with `;`), or the trailing tail expression.
            let expr = self.expr()?;

            if matches!(self.peek(), Tok::Eq) {
                // Assignment: `place = value;`
                self.bump();
                let value = self.expr()?;
                let span = expr.span().merge(value.span());
                self.expect(&Tok::Semi, "`;` after assignment")?;
                stmts.push(Stmt::Assign {
                    target: expr,
                    value,
                    span,
                });
                continue;
            }

            if self.eat(&Tok::Semi) {
                stmts.push(Stmt::Expr(expr));
                continue;
            }

            // No semicolon. If we're at `}`, this is the tail expression.
            if self.at(&Tok::RBrace) {
                tail = Some(Box::new(expr));
                break;
            }

            // Block-like expressions are allowed as statements without `;`.
            if is_block_like(&expr) {
                stmts.push(Stmt::Expr(expr));
                continue;
            }

            return Err(self.err(format!(
                "expected `;` or `}}` after expression, found {}",
                describe(self.peek())
            )));
        }

        self.expect(&Tok::RBrace, "`}`")?;
        Ok(Block {
            stmts,
            tail,
            span: start.merge(self.prev_span()),
        })
    }

    /// Parse a `let` binding, producing one or more statements. A tuple
    /// destructuring `let (a, b) = e;` desugars to a hidden temp plus one
    /// binding per element (`let a = __let.0; ...`).
    fn let_stmt(&mut self) -> PResult<Vec<Stmt>> {
        let start = self.span();
        self.expect(&Tok::Let, "`let`")?;
        let mutable = self.eat(&Tok::Mut);

        // Tuple destructuring: `let (a, b, ...) = init;`
        if self.at(&Tok::LParen) {
            self.bump();
            let mut names = Vec::new();
            while !self.at(&Tok::RParen) {
                let (n, _) = self.ident()?;
                names.push(n);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen, "`)` in destructuring let")?;
            self.expect(&Tok::Eq, "`=` in let binding")?;
            let init = self.expr()?;
            self.expect(&Tok::Semi, "`;` after let binding")?;
            let span = start.merge(self.prev_span());

            let tmp = format!("__let{}", span.start);
            let mut out = vec![Stmt::Let {
                name: tmp.clone(),
                mutable: false,
                ty: None,
                init,
                span,
            }];
            for (i, n) in names.into_iter().enumerate() {
                out.push(Stmt::Let {
                    name: n,
                    mutable,
                    ty: None,
                    init: Expr::TupleField {
                        base: Box::new(Expr::Ident {
                            name: tmp.clone(),
                            span,
                        }),
                        index: i as u64,
                        span,
                    },
                    span,
                });
            }
            return Ok(out);
        }

        let (name, _) = self.ident()?;
        let ty = if self.eat(&Tok::Colon) {
            Some(self.type_expr()?)
        } else {
            None
        };
        self.expect(&Tok::Eq, "`=` in let binding")?;
        let init = self.expr()?;
        self.expect(&Tok::Semi, "`;` after let binding")?;
        Ok(vec![Stmt::Let {
            name,
            mutable,
            ty,
            init,
            span: start.merge(self.prev_span()),
        }])
    }

    fn while_stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        self.expect(&Tok::While, "`while`")?;
        let cond = self.expr_no_struct()?;
        let body = self.block()?;
        Ok(Stmt::While {
            cond,
            body,
            span: start.merge(self.prev_span()),
        })
    }

    fn for_stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        self.expect(&Tok::For, "`for`")?;
        let (var, _) = self.ident()?;
        self.expect(&Tok::In, "`in`")?;
        let lo = self.expr_no_struct()?;
        self.expect(&Tok::DotDot, "`..` in for-range")?;
        let hi = self.expr_no_struct()?;
        let body = self.block()?;
        Ok(Stmt::For {
            var,
            lo,
            hi,
            body,
            span: start.merge(self.prev_span()),
        })
    }

    fn return_stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        self.expect(&Tok::Return, "`return`")?;
        let value = if self.at(&Tok::Semi) {
            None
        } else {
            Some(self.expr()?)
        };
        self.expect(&Tok::Semi, "`;` after return")?;
        Ok(Stmt::Return {
            value,
            span: start.merge(self.prev_span()),
        })
    }

    // ---- expressions (Pratt) -------------------------------------------

    fn expr(&mut self) -> PResult<Expr> {
        self.expr_bp(0, true)
    }

    /// Parse an expression but forbid a trailing struct literal `Name { ... }`.
    /// Used in `if`/`while`/`for`/`match` heads so the `{` is unambiguously the
    /// body.
    fn expr_no_struct(&mut self) -> PResult<Expr> {
        self.expr_bp(0, false)
    }

    fn expr_bp(&mut self, min_bp: u8, allow_struct: bool) -> PResult<Expr> {
        let mut lhs = self.prefix(allow_struct)?;

        loop {
            let op = match binop(self.peek()) {
                Some(op) => op,
                None => break,
            };
            let (lbp, rbp) = infix_bp(op);
            if lbp < min_bp {
                break;
            }
            self.bump();
            let rhs = self.expr_bp(rbp, allow_struct)?;
            let span = lhs.span().merge(rhs.span());
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn prefix(&mut self, allow_struct: bool) -> PResult<Expr> {
        let start = self.span();
        match self.peek() {
            Tok::Minus => {
                self.bump();
                let expr = self.prefix(allow_struct)?;
                Ok(Expr::Unary {
                    op: UnOp::Neg,
                    span: start.merge(expr.span()),
                    expr: Box::new(expr),
                })
            }
            Tok::Bang => {
                self.bump();
                let expr = self.prefix(allow_struct)?;
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    span: start.merge(expr.span()),
                    expr: Box::new(expr),
                })
            }
            _ => self.postfix(allow_struct),
        }
    }

    fn postfix(&mut self, allow_struct: bool) -> PResult<Expr> {
        let mut expr = self.atom(allow_struct)?;
        loop {
            match self.peek() {
                Tok::LParen => {
                    self.bump();
                    let mut args = Vec::new();
                    while !self.at(&Tok::RParen) {
                        args.push(self.expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(&Tok::RParen, "`)`")?;
                    let span = expr.span().merge(self.prev_span());
                    expr = Expr::Call {
                        callee: Box::new(expr),
                        args,
                        span,
                    };
                }
                Tok::Dot => {
                    self.bump();
                    // `expr.0` is tuple access; `expr.field` is struct access.
                    if let Tok::Int(idx) = self.peek().clone() {
                        self.bump();
                        let span = expr.span().merge(self.prev_span());
                        expr = Expr::TupleField {
                            base: Box::new(expr),
                            index: idx as u64,
                            span,
                        };
                    } else {
                        let (field, _) = self.ident()?;
                        let span = expr.span().merge(self.prev_span());
                        expr = Expr::Field {
                            base: Box::new(expr),
                            field,
                            span,
                        };
                    }
                }
                Tok::LBracket => {
                    self.bump();
                    let index = self.expr()?;
                    self.expect(&Tok::RBracket, "`]`")?;
                    let span = expr.span().merge(self.prev_span());
                    expr = Expr::Index {
                        base: Box::new(expr),
                        index: Box::new(index),
                        span,
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn atom(&mut self, allow_struct: bool) -> PResult<Expr> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Int(value) => {
                self.bump();
                Ok(Expr::Int { value, span })
            }
            Tok::True => {
                self.bump();
                Ok(Expr::Bool { value: true, span })
            }
            Tok::False => {
                self.bump();
                Ok(Expr::Bool { value: false, span })
            }
            Tok::Ident(_) => {
                let segs = self.name_path()?;
                let span = span.merge(self.prev_span());
                // Struct literal `Name { field: expr, ... }` — only when allowed
                // and the final segment looks like a type (starts uppercase) to
                // avoid ambiguity with a following block.
                let last_upper = segs
                    .last()
                    .and_then(|s| s.chars().next())
                    .map_or(false, |c| c.is_uppercase());
                if allow_struct && self.at(&Tok::LBrace) && last_upper {
                    return self.struct_lit(segs, span);
                }
                if segs.len() == 1 {
                    Ok(Expr::Ident {
                        name: segs.into_iter().next().unwrap(),
                        span,
                    })
                } else {
                    Ok(Expr::Path { segments: segs, span })
                }
            }
            Tok::LParen => {
                self.bump();
                if self.eat(&Tok::RParen) {
                    // `()` unit literal — represent as an empty block value.
                    return Ok(Expr::Block(Block {
                        stmts: vec![],
                        tail: None,
                        span: span.merge(self.prev_span()),
                    }));
                }
                // Either a parenthesized expression `(e)` or a tuple `(e0, e1, ...)`.
                let first = self.expr()?;
                if self.at(&Tok::Comma) {
                    let mut elems = vec![first];
                    while self.eat(&Tok::Comma) {
                        if self.at(&Tok::RParen) {
                            break; // allow trailing comma
                        }
                        elems.push(self.expr()?);
                    }
                    self.expect(&Tok::RParen, "`)`")?;
                    Ok(Expr::Tuple {
                        elems,
                        span: span.merge(self.prev_span()),
                    })
                } else {
                    self.expect(&Tok::RParen, "`)`")?;
                    Ok(first)
                }
            }
            Tok::LBracket => {
                self.bump();
                let mut elems = Vec::new();
                while !self.at(&Tok::RBracket) {
                    elems.push(self.expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RBracket, "`]`")?;
                Ok(Expr::Array {
                    elems,
                    span: span.merge(self.prev_span()),
                })
            }
            Tok::If => self.if_expr(),
            Tok::Match => self.match_expr(),
            Tok::LBrace => Ok(Expr::Block(self.block()?)),
            other => Err(self.err(format!(
                "expected an expression, found {}",
                describe(&other)
            ))),
        }
    }

    fn struct_lit(&mut self, path: Path, start: Span) -> PResult<Expr> {
        self.expect(&Tok::LBrace, "`{`")?;
        let mut fields = Vec::new();
        while !self.at(&Tok::RBrace) {
            let (fname, _) = self.ident()?;
            self.expect(&Tok::Colon, "`:` in struct field")?;
            let value = self.expr()?;
            fields.push((fname, value));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(Expr::StructLit {
            path,
            fields,
            span: start.merge(self.prev_span()),
        })
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        let start = self.span();
        self.expect(&Tok::If, "`if`")?;
        let cond = self.expr_no_struct()?;
        let then_branch = self.block()?;
        let else_branch = if self.eat(&Tok::Else) {
            if self.at(&Tok::If) {
                Some(Box::new(ElseBranch::If(self.if_expr()?)))
            } else {
                Some(Box::new(ElseBranch::Block(self.block()?)))
            }
        } else {
            None
        };
        Ok(Expr::If {
            cond: Box::new(cond),
            then_branch,
            else_branch,
            span: start.merge(self.prev_span()),
        })
    }

    fn match_expr(&mut self) -> PResult<Expr> {
        let start = self.span();
        self.expect(&Tok::Match, "`match`")?;
        let scrutinee = self.expr_no_struct()?;
        self.expect(&Tok::LBrace, "`{`")?;
        let mut arms = Vec::new();
        while !self.at(&Tok::RBrace) {
            let astart = self.span();
            let pattern = self.pattern()?;
            let guard = if self.eat(&Tok::If) {
                Some(self.expr_no_struct()?)
            } else {
                None
            };
            self.expect(&Tok::FatArrow, "`=>` in match arm")?;
            let body = self.expr()?;
            arms.push(MatchArm {
                pattern,
                guard,
                span: astart.merge(self.prev_span()),
                body,
            });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            span: start.merge(self.prev_span()),
        })
    }

    /// Parse a full pattern, including top-level or-patterns `p | q | ...`.
    fn pattern(&mut self) -> PResult<Pattern> {
        let start = self.span();
        let first = self.pattern_primary()?;
        if self.at(&Tok::Pipe) {
            let mut alts = vec![first];
            while self.eat(&Tok::Pipe) {
                alts.push(self.pattern_primary()?);
            }
            Ok(Pattern::Or {
                alts,
                span: start.merge(self.prev_span()),
            })
        } else {
            Ok(first)
        }
    }

    fn pattern_primary(&mut self) -> PResult<Pattern> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Ident(name) if name == "_" => {
                self.bump();
                Ok(Pattern::Wildcard { span })
            }
            Tok::Ident(_) => {
                let segs = self.name_path()?;
                let last_upper = segs
                    .last()
                    .and_then(|s| s.chars().next())
                    .map_or(false, |c| c.is_uppercase());
                let is_variant = self.at(&Tok::LParen) || segs.len() > 1 || last_upper;
                if is_variant {
                    let mut subpatterns = Vec::new();
                    if self.eat(&Tok::LParen) {
                        while !self.at(&Tok::RParen) {
                            subpatterns.push(self.pattern()?);
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                        }
                        self.expect(&Tok::RParen, "`)`")?;
                    }
                    Ok(Pattern::Variant {
                        path: segs,
                        subpatterns,
                        span: span.merge(self.prev_span()),
                    })
                } else {
                    Ok(Pattern::Binding {
                        name: segs.into_iter().next().unwrap(),
                        span,
                    })
                }
            }
            Tok::True => {
                self.bump();
                Ok(Pattern::Bool { value: true, span })
            }
            Tok::False => {
                self.bump();
                Ok(Pattern::Bool { value: false, span })
            }
            // Tuple pattern `(p0, p1, ...)`; `(p)` is just `p`; `()` matches unit.
            Tok::LParen => {
                self.bump();
                let mut elems = Vec::new();
                let mut trailing_comma = false;
                while !self.at(&Tok::RParen) {
                    elems.push(self.pattern()?);
                    if self.eat(&Tok::Comma) {
                        trailing_comma = true;
                    } else {
                        trailing_comma = false;
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)`")?;
                if elems.len() == 1 && !trailing_comma {
                    Ok(elems.into_iter().next().unwrap())
                } else {
                    Ok(Pattern::Tuple {
                        elems,
                        span: span.merge(self.prev_span()),
                    })
                }
            }
            // Integer literal or range pattern.
            Tok::Minus | Tok::Int(_) => {
                let lo = self.int_signed_pattern()?;
                if self.at(&Tok::DotDot) || self.at(&Tok::DotDotEq) {
                    let inclusive = self.eat(&Tok::DotDotEq);
                    if !inclusive {
                        self.expect(&Tok::DotDot, "`..` or `..=` in range pattern")?;
                    }
                    let hi = self.int_signed_pattern()?;
                    Ok(Pattern::Range {
                        lo,
                        hi,
                        inclusive,
                        span: span.merge(self.prev_span()),
                    })
                } else {
                    Ok(Pattern::Int {
                        value: lo,
                        span: span.merge(self.prev_span()),
                    })
                }
            }
            other => Err(self.err(format!("expected a pattern, found {}", describe(&other)))),
        }
    }

    /// Parse an optionally-negated integer literal (a range/int pattern bound).
    fn int_signed_pattern(&mut self) -> PResult<i128> {
        if self.eat(&Tok::Minus) {
            Ok(-self.int_literal("integer pattern")?)
        } else {
            self.int_literal("integer pattern")
        }
    }
}

/// `if`/`match`/`{...}` may appear as statements without a trailing `;`.
fn is_block_like(e: &Expr) -> bool {
    matches!(e, Expr::If { .. } | Expr::Match { .. } | Expr::Block(_))
}

fn binop(t: &Tok) -> Option<BinOp> {
    Some(match t {
        Tok::Plus => BinOp::Add,
        Tok::Minus => BinOp::Sub,
        Tok::Star => BinOp::Mul,
        Tok::Slash => BinOp::Div,
        Tok::Percent => BinOp::Rem,
        Tok::AndAnd => BinOp::And,
        Tok::OrOr => BinOp::Or,
        Tok::Amp => BinOp::BitAnd,
        Tok::Pipe => BinOp::BitOr,
        Tok::Caret => BinOp::BitXor,
        Tok::Shl => BinOp::Shl,
        Tok::Shr => BinOp::Shr,
        Tok::EqEq => BinOp::Eq,
        Tok::Ne => BinOp::Ne,
        Tok::Lt => BinOp::Lt,
        Tok::Le => BinOp::Le,
        Tok::Gt => BinOp::Gt,
        Tok::Ge => BinOp::Ge,
        _ => return None,
    })
}

/// Binding powers for infix operators (left, right). Higher binds tighter.
fn infix_bp(op: BinOp) -> (u8, u8) {
    use BinOp::*;
    match op {
        Or => (1, 2),
        And => (3, 4),
        Eq | Ne | Lt | Le | Gt | Ge => (5, 6),
        BitOr => (7, 8),
        BitXor => (9, 10),
        BitAnd => (11, 12),
        Shl | Shr => (13, 14),
        Add | Sub => (15, 16),
        Mul | Div | Rem => (17, 18),
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Eof => "end of input".to_string(),
        Tok::Ident(s) => format!("identifier `{}`", s),
        Tok::Int(v) => format!("integer `{}`", v),
        other => format!("`{:?}`", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn parse_src(src: &str) -> PResult<Program> {
        parse(&lex(src).unwrap())
    }

    #[test]
    fn parses_milestone() {
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
        let prog = parse_src(src).unwrap();
        assert_eq!(prog.items.len(), 4);
    }

    #[test]
    fn precedence_is_correct() {
        let prog = parse_src("fn f() -> u8 { 1 + 2 * 3 }").unwrap();
        if let Item::Func(f) = &prog.items[0] {
            let tail = f.body.tail.as_ref().unwrap();
            // Should parse as 1 + (2 * 3): top op is Add.
            if let Expr::Binary { op, .. } = tail.as_ref() {
                assert_eq!(*op, BinOp::Add);
            } else {
                panic!("expected binary");
            }
        } else {
            panic!("expected func");
        }
    }

    #[test]
    fn struct_and_field() {
        let prog = parse_src(
            "struct P { x: u32, y: u32 } fn f() -> u32 { let p = P { x: 1, y: 2 }; p.x }",
        )
        .unwrap();
        assert_eq!(prog.items.len(), 2);
    }

    #[test]
    fn for_and_while_and_array() {
        let prog = parse_src(
            "fn f() -> u32 { let mut s = 0; let a = [1, 2, 3]; for i in 0..3 { s = s + a[i]; } while s > 0 { s = s - 1; } s }",
        )
        .unwrap();
        assert_eq!(prog.items.len(), 1);
    }

    #[test]
    fn parse_error_is_structured_not_panic() {
        let err = parse_src("fn f( {").unwrap_err();
        assert_eq!(err.stage, Stage::Parse);
    }

    #[test]
    fn parse_error_on_missing_semi() {
        let err = parse_src("fn f() -> u8 { let x = 1 x }").unwrap_err();
        assert_eq!(err.stage, Stage::Parse);
    }
}

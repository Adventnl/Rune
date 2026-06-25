//! # Rune Abstract Syntax Tree (AST)
//!
//! The AST is the *untyped surface representation* produced by the parser. It
//! mirrors the concrete syntax closely: names are still strings, integer
//! literals are not yet committed to a width, and no semantic checks have been
//! applied. The typechecker ([`crate::typeck`]) consumes the AST and lowers it
//! into the typed core IR ([`crate::ir`]), which is the stable contract for the
//! interpreter and the (future) HDL backend.
//!
//! Every node carries a [`Span`] for diagnostics. The AST deliberately keeps no
//! type information beyond optional user annotations.

use crate::span::Span;

/// A whole parsed source file: an ordered list of top-level items.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub items: Vec<Item>,
}

/// A top-level (or in-module) declaration.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Func(Func),
    Struct(StructDef),
    Enum(EnumDef),
    /// A module: a named namespace of items. Inline (`mod m { ... }`) carries
    /// `Some(items)`; a file module declaration (`mod m;`) carries `None` and is
    /// resolved to a sibling file by the module loader before typechecking.
    Mod(ModDef),
    /// A `use path::to::item;` (or `use path::*;`) import.
    Use(UseDecl),
}

/// A dotted/`::`-separated name path, e.g. `math::clamp` → `["math", "clamp"]`.
pub type Path = Vec<String>;

/// A module declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct ModDef {
    pub name: String,
    /// `Some` for inline modules; `None` for `mod name;` file declarations.
    pub items: Option<Vec<Item>>,
    pub span: Span,
}

/// A `use` import: brings a path's item (or, with `glob`, all of a module's
/// items) into the current module's scope under their simple names.
#[derive(Clone, Debug, PartialEq)]
pub struct UseDecl {
    pub path: Path,
    /// `true` for `use path::*;`.
    pub glob: bool,
    pub span: Span,
}

/// A function definition: `fn name(params) -> ret { body }`.
///
/// A missing `-> ret` means the function returns the unit type (`()`), used for
/// statement-style functions like `main`.
#[derive(Clone, Debug, PartialEq)]
pub struct Func {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Block,
    pub span: Span,
}

/// A single function parameter `name: type`.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

/// A `struct Name { field: Type, ... }` definition. Field order is significant
/// (it determines the IR field layout).
#[derive(Clone, Debug, PartialEq)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldDef {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

/// An `enum Name { Variant(T, ...), Unit, ... }` definition. Variant order
/// determines the IR tag values (first variant = tag 0).
#[derive(Clone, Debug, PartialEq)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<VariantDef>,
    pub span: Span,
}

/// A single enum variant. `fields` is empty for unit-like variants.
#[derive(Clone, Debug, PartialEq)]
pub struct VariantDef {
    pub name: String,
    pub fields: Vec<TypeExpr>,
    pub span: Span,
}

/// A syntactic type annotation, e.g. `bit<8>`, `[u32; 4]`, `Shape`.
///
/// This is the *surface* type; the typechecker resolves it to [`crate::ir::Type`].
#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    /// `bool`
    Bool,
    /// One of `i8 i16 i32 i64 u8 u16 u32 u64`.
    Int { signed: bool, width: u32 },
    /// `bit<N>` — explicit bit-vector with wrapping semantics.
    Bit { width: u32 },
    /// `[T; N]` — fixed-size array.
    Array { elem: Box<TypeExpr>, len: u64 },
    /// A named type, possibly module-qualified (resolved later to a struct or
    /// enum), e.g. `Shape` or `geom::Point`.
    Named(Path),
    /// The unit type, written `()`.
    Unit,
}

/// A `{ ... }` block: a sequence of statements with an optional trailing
/// expression that becomes the block's value (Rust-style tail expressions).
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

/// A statement inside a block.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// `let [mut] name [: T] = expr;`
    Let {
        name: String,
        mutable: bool,
        ty: Option<TypeExpr>,
        init: Expr,
        span: Span,
    },
    /// `place = expr;`
    Assign { target: Expr, value: Expr, span: Span },
    /// `while cond { body }`
    While { cond: Expr, body: Block, span: Span },
    /// `for name in lo..hi { body }` — a bounded integer range loop.
    For {
        var: String,
        lo: Expr,
        hi: Expr,
        body: Block,
        span: Span,
    },
    /// `return [expr];`
    Return { value: Option<Expr>, span: Span },
    /// An expression evaluated for its effect, e.g. `print(x);`.
    Expr(Expr),
}

/// An expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// Integer literal (width not yet fixed; resolved by the typechecker).
    Int { value: i128, span: Span },
    /// Boolean literal.
    Bool { value: bool, span: Span },
    /// A bare name: a local variable, function, or unit-like enum variant.
    Ident { name: String, span: Span },
    /// A module-qualified path used as a value, e.g. `math::PI` or a unit-like
    /// variant `Shape::Dot`. Always has 2+ segments (single names are `Ident`).
    Path { segments: Path, span: Span },
    /// Unary operation.
    Unary { op: UnOp, expr: Box<Expr>, span: Span },
    /// Binary operation.
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    /// A call `callee(args)`. The callee is parsed as an expression but must
    /// resolve to a function name or an enum variant constructor.
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        span: Span,
    },
    /// Field access `expr.field`.
    Field {
        base: Box<Expr>,
        field: String,
        span: Span,
    },
    /// Array indexing `expr[index]`.
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    /// Array literal `[a, b, c]`.
    Array { elems: Vec<Expr>, span: Span },
    /// Struct literal `Name { field: expr, ... }` (name may be path-qualified).
    StructLit {
        path: Path,
        fields: Vec<(String, Expr)>,
        span: Span,
    },
    /// `if cond { then } else { else }` used as an expression.
    If {
        cond: Box<Expr>,
        then_branch: Block,
        else_branch: Option<Box<ElseBranch>>,
        span: Span,
    },
    /// `match scrutinee { arms }`.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
        span: Span,
    },
    /// A bare block used as an expression `{ ... }`.
    Block(Block),
}

/// The `else` branch of an `if`: either a final block or a chained `else if`.
#[derive(Clone, Debug, PartialEq)]
pub enum ElseBranch {
    Block(Block),
    If(Expr),
}

/// A single `pattern => body` arm of a `match`.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

/// A match pattern. v1 keeps patterns intentionally small but expressive enough
/// for exhaustiveness checking over enums and bools.
#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    /// `_`
    Wildcard { span: Span },
    /// A binding that captures the whole scrutinee, e.g. `x`.
    Binding { name: String, span: Span },
    /// An integer literal pattern.
    Int { value: i128, span: Span },
    /// A boolean literal pattern.
    Bool { value: bool, span: Span },
    /// An enum variant pattern `Variant(sub, ...)` (or `Variant` for unit-like),
    /// where the variant may be path-qualified, e.g. `Shape::Rect(w, h)`.
    Variant {
        path: Path,
        subpatterns: Vec<Pattern>,
        span: Span,
    },
}

impl Expr {
    /// The source span of this expression.
    pub fn span(&self) -> Span {
        match self {
            Expr::Int { span, .. }
            | Expr::Bool { span, .. }
            | Expr::Ident { span, .. }
            | Expr::Path { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Call { span, .. }
            | Expr::Field { span, .. }
            | Expr::Index { span, .. }
            | Expr::Array { span, .. }
            | Expr::StructLit { span, .. }
            | Expr::If { span, .. }
            | Expr::Match { span, .. } => *span,
            Expr::Block(b) => b.span,
        }
    }
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard { span }
            | Pattern::Binding { span, .. }
            | Pattern::Int { span, .. }
            | Pattern::Bool { span, .. }
            | Pattern::Variant { span, .. } => *span,
        }
    }
}

/// Unary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// Arithmetic negation `-x`.
    Neg,
    /// Logical/bitwise NOT `!x`.
    Not,
}

/// Binary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    And, // logical &&
    Or,  // logical ||
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

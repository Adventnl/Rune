//! # Rune Typed Core IR
//!
//! This module defines the **typed core intermediate representation** — the
//! single stable contract that every later stage consumes. Today the
//! tree-walking interpreter ([`crate::interp`]) evaluates it; in the future an
//! HDL backend is intended to lower the synthesizable subset of it to
//! Verilog/HDL. The HDL-subset analysis pass ([`crate::hdl`]) already reads it.
//!
//! ## Invariants (see also `docs/ir.md`)
//!
//! 1. **Fully typed.** Every [`Expr`] carries its resolved [`Type`]. The
//!    typechecker is the *only* producer of IR; by the time IR exists, all
//!    names are resolved, all widths are fixed, and all `match`es are
//!    exhaustive.
//! 2. **Deterministic, no UB.** There is no undefined behavior anywhere.
//!    Integer overflow is *wrapping* (two's complement) for every integer-like
//!    type, so the interpreter and any future hardware target agree bit-for-bit.
//!    Division/remainder by zero is a defined, reported runtime trap — never UB.
//! 3. **Explicit bit widths.** [`Type::Bit`] carries its width `N`; arithmetic
//!    wraps modulo `2^N`. This is the HDL bridge and its semantics are exact.
//! 4. **Value semantics, no heap.** Arrays, structs, and enums are values.
//!    There is no aliasing, no references, no allocation in the core.
//! 5. **Small and explicit.** Control flow is structured (`if`/`while`/`for`/
//!    `match`); names are resolved; enum variants are referred to by numeric
//!    tag. This keeps the IR SSA-friendly and HDL-lowerable.
//!
//! The IR is *frozen*: Phase-2 features consume it but never modify it.

use std::collections::BTreeMap;

/// A resolved Rune type. Unlike [`crate::ast::TypeExpr`], every named type here
/// is known to exist and integer widths are committed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    /// `bool`
    Bool,
    /// The unit type `()` — the result of statements and `()`-returning fns.
    Unit,
    /// A fixed-width machine integer (`i8..i64`, `u8..u64`).
    Int(IntTy),
    /// `bit<N>` — an explicit bit-vector with wrapping arithmetic. `1 <= N <= 128`.
    Bit(u32),
    /// `[T; N]` — a fixed-size array.
    Array(Box<Type>, u64),
    /// A named struct type.
    Struct(String),
    /// A named (tagged) enum type.
    Enum(String),
    /// An anonymous tuple type `(T0, T1, ...)`. Always has 2+ elements (a
    /// 1-tuple is just the element; the 0-tuple is [`Type::Unit`]).
    Tuple(Vec<Type>),
}

impl Type {
    /// True if this type is integer-like (`IntTy` or `bit<N>`), the set of
    /// types arithmetic and comparison operators accept.
    pub fn is_integer(&self) -> bool {
        matches!(self, Type::Int(_) | Type::Bit(_))
    }

    /// The bit width of an integer-like type, if any.
    pub fn bit_width(&self) -> Option<u32> {
        match self {
            Type::Int(i) => Some(i.width),
            Type::Bit(n) => Some(*n),
            _ => None,
        }
    }
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Bool => write!(f, "bool"),
            Type::Unit => write!(f, "()"),
            Type::Int(i) => write!(f, "{}{}", if i.signed { "i" } else { "u" }, i.width),
            Type::Bit(n) => write!(f, "bit<{}>", n),
            Type::Array(t, n) => write!(f, "[{}; {}]", t, n),
            Type::Struct(n) | Type::Enum(n) => write!(f, "{}", n),
            Type::Tuple(ts) => {
                let parts: Vec<String> = ts.iter().map(|t| t.to_string()).collect();
                write!(f, "({})", parts.join(", "))
            }
        }
    }
}

/// A fixed-width machine integer type, e.g. `i32` or `u8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IntTy {
    pub signed: bool,
    /// One of 8, 16, 32, 64.
    pub width: u32,
}

/// A complete typed program: the unit of typechecking and hot reload.
///
/// Definitions are kept in stable, name-keyed maps so that the hot-reload
/// registry can swap a single definition without disturbing the rest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Module {
    pub structs: BTreeMap<String, StructDef>,
    pub enums: BTreeMap<String, EnumDef>,
    pub funcs: BTreeMap<String, Func>,
}

/// A typed struct definition with an ordered field layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<Field>,
}

impl StructDef {
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: Type,
}

/// A typed enum definition. Variant order defines the tag: the first variant is
/// tag `0`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<Variant>,
}

impl EnumDef {
    pub fn variant_tag(&self, name: &str) -> Option<usize> {
        self.variants.iter().position(|v| v.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub name: String,
    /// Payload field types, in order. Empty for unit-like variants.
    pub fields: Vec<Type>,
}

/// A typed function definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Func {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Type,
    pub body: Block,
}

/// The function signature only — used by hot reload to decide whether a swapped
/// definition is compatible with live state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub params: Vec<Type>,
    pub ret: Type,
}

impl Func {
    pub fn signature(&self) -> Signature {
        Signature {
            params: self.params.iter().map(|p| p.ty.clone()).collect(),
            ret: self.ret.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub ty: Type,
}

/// A typed block: a sequence of statements and an optional tail expression whose
/// value becomes the block's value.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
    /// The type the block evaluates to (the tail's type, or `Unit`).
    pub ty: Type,
}

/// A typed statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// Introduce a local binding. `mutable` controls whether reassignment is
    /// permitted (already checked by the typechecker).
    Let {
        name: String,
        mutable: bool,
        ty: Type,
        init: Expr,
    },
    /// Assign to an existing place (local, field, or array element).
    Assign { place: Place, value: Expr },
    /// `while cond { body }`. `cond` is always `bool`.
    While { cond: Expr, body: Block },
    /// `for var in lo..hi { body }` — a bounded loop over an integer range.
    /// `var` takes the type of `lo`/`hi` (which match). The range is half-open.
    For {
        var: String,
        ty: Type,
        lo: Expr,
        hi: Expr,
        body: Block,
    },
    /// `return value;`
    Return { value: Option<Expr> },
    /// An expression evaluated for effect.
    Expr(Expr),
}

/// An assignable location.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    /// A local variable.
    Local { name: String, ty: Type },
    /// A struct field of a place.
    Field {
        base: Box<Place>,
        index: usize,
        ty: Type,
    },
    /// An array element of a place.
    Index {
        base: Box<Place>,
        index: Box<Expr>,
        ty: Type,
    },
    /// A tuple element of a place, e.g. `t.0`.
    TupleField {
        base: Box<Place>,
        index: usize,
        ty: Type,
    },
}

impl Place {
    pub fn ty(&self) -> &Type {
        match self {
            Place::Local { ty, .. }
            | Place::Field { ty, .. }
            | Place::Index { ty, .. }
            | Place::TupleField { ty, .. } => ty,
        }
    }
}

/// A typed expression: an [`ExprKind`] paired with its resolved [`Type`].
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub ty: Type,
}

impl Expr {
    pub fn new(kind: ExprKind, ty: Type) -> Self {
        Expr { kind, ty }
    }
}

/// The shape of a typed expression. The associated [`Type`] lives on the
/// enclosing [`Expr`].
#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    /// An integer literal. The exact type (and thus the wrapping width and
    /// signedness) is on `Expr::ty`.
    Int(i128),
    /// A boolean literal.
    Bool(bool),
    /// The unit value `()`.
    Unit,
    /// A reference to a local variable or parameter.
    Local(String),
    /// Unary operation.
    Unary { op: UnOp, expr: Box<Expr> },
    /// Binary operation. Operand and result types are determined and checked.
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// A direct call to a named function.
    Call { func: String, args: Vec<Expr> },
    /// The built-in `print(x)`.
    Print { arg: Box<Expr> },
    /// Construct a struct value with fields in definition order.
    MakeStruct { name: String, fields: Vec<Expr> },
    /// Construct an enum value. `tag` indexes into the enum's variant list.
    MakeEnum {
        name: String,
        tag: usize,
        args: Vec<Expr>,
    },
    /// Construct a tuple value from its elements.
    MakeTuple(Vec<Expr>),
    /// Read a struct field by resolved index.
    Field { base: Box<Expr>, index: usize },
    /// Read a tuple element by position, e.g. `t.0`.
    TupleField { base: Box<Expr>, index: usize },
    /// Index into an array.
    Index { base: Box<Expr>, index: Box<Expr> },
    /// Array literal.
    Array(Vec<Expr>),
    /// `if cond { then } else { else }` as an expression. When used in
    /// statement position the `else` block may be unit-typed/empty.
    If {
        cond: Box<Expr>,
        then_branch: Block,
        else_branch: Block,
    },
    /// An exhaustive `match`. Exhaustiveness is guaranteed by the typechecker.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    /// A block used as an expression.
    Block(Block),
}

/// One arm of a typed `match`. An optional `guard` (a `bool` expression) must
/// evaluate true for the arm to fire.
#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
}

/// A typed match pattern. Bindings introduced here are typed.
#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    /// `_`
    Wildcard,
    /// Bind the whole scrutinee to `name`.
    Binding { name: String, ty: Type },
    /// Integer literal.
    Int(i128),
    /// Boolean literal.
    Bool(bool),
    /// Integer range. `inclusive` selects `lo..=hi` vs the half-open `lo..hi`.
    Range {
        lo: i128,
        hi: i128,
        inclusive: bool,
    },
    /// `Variant(sub, ...)` — `tag` indexes the enum's variants and `subpatterns`
    /// match its payload fields positionally.
    Variant {
        enum_name: String,
        tag: usize,
        subpatterns: Vec<Pattern>,
    },
    /// Tuple pattern, matching each element positionally.
    Tuple(Vec<Pattern>),
    /// Or-pattern: matches if any alternative matches. All alternatives bind the
    /// same names with the same types.
    Or(Vec<Pattern>),
}

/// Unary operators (resolved; semantics fixed by operand type).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// Two's-complement negation (wrapping) on integer-like types.
    Neg,
    /// Logical NOT on `bool`, bitwise NOT on integer-like types.
    Not,
}

/// Binary operators (resolved).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    /// Wrapping add/sub/mul on integer-like types.
    Add,
    Sub,
    Mul,
    /// Division and remainder. Trap (defined runtime error) on divisor `0`.
    Div,
    Rem,
    /// Short-circuiting logical operators on `bool`.
    And,
    Or,
    /// Bitwise operators on integer-like types.
    BitAnd,
    BitOr,
    BitXor,
    /// Shifts on integer-like types; shift amount is taken modulo the width to
    /// stay deterministic.
    Shl,
    Shr,
    /// Comparisons; produce `bool`.
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    /// True if the operator yields a `bool` regardless of operand type.
    pub fn is_comparison(&self) -> bool {
        matches!(
            self,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        )
    }

    /// True for the short-circuiting logical operators.
    pub fn is_logical(&self) -> bool {
        matches!(self, BinOp::And | BinOp::Or)
    }
}

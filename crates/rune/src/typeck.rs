//! # Typechecker
//!
//! Lowers the untyped [`ast::Program`] into the typed core [`ir::Module`]. This
//! is the only producer of IR: by the time IR exists, every name is resolved,
//! every integer width is committed, and every `match` is proven exhaustive.
//!
//! ## Integer literal inference
//!
//! Rune has no implicit integer conversions, but bare integer literals must
//! still acquire a concrete width. The checker threads an *expected type* into
//! expression checking (from annotations, parameter types, return types, struct
//! fields, and enum payloads). In a binary operation, a literal takes its type
//! from the non-literal operand; when no context is available a literal
//! defaults to `i32`.
//!
//! ## Errors
//!
//! Every failure is a located [`Diagnostic`]. Each top-level item is checked
//! independently so an error in one function does not hide errors in others.

use crate::ast::{self, BinOp, UnOp};
use crate::diagnostic::{Diagnostic, Stage};
use crate::ir::{self, IntTy, Type};
use crate::span::Span;
use std::collections::BTreeMap;
use std::collections::HashMap;

const I32: Type = Type::Int(IntTy {
    signed: true,
    width: 32,
});

/// Typecheck a program, producing a typed [`ir::Module`] or a list of
/// diagnostics.
pub fn check(program: &ast::Program) -> Result<ir::Module, Vec<Diagnostic>> {
    let mut cx = Checker::new();
    cx.collect(program);
    if !cx.diags.is_empty() {
        return Err(cx.diags);
    }

    let mut module = ir::Module::default();
    module.structs = cx.structs.clone();
    module.enums = cx.enums.clone();

    for item in &program.items {
        if let ast::Item::Func(f) = item {
            match cx.check_func(f) {
                Ok(func) => {
                    module.funcs.insert(func.name.clone(), func);
                }
                Err(d) => cx.diags.push(d),
            }
        }
    }

    if cx.diags.is_empty() {
        Ok(module)
    } else {
        Err(cx.diags)
    }
}

/// Typecheck a single function against an existing set of definitions. Used by
/// hot reload to re-check just one changed definition.
pub fn check_func_against(
    func: &ast::Func,
    structs: &BTreeMap<String, ir::StructDef>,
    enums: &BTreeMap<String, ir::EnumDef>,
    funcs: &BTreeMap<String, ir::Func>,
) -> Result<ir::Func, Vec<Diagnostic>> {
    let mut cx = Checker::new();
    cx.structs = structs.clone();
    cx.enums = enums.clone();
    for (name, e) in enums {
        for (tag, v) in e.variants.iter().enumerate() {
            cx.variants.insert(v.name.clone(), (name.clone(), tag));
        }
    }
    for (name, f) in funcs {
        cx.funcs.insert(name.clone(), f.signature());
    }
    cx.check_func(func).map_err(|d| vec![d])
}

struct Checker {
    structs: BTreeMap<String, ir::StructDef>,
    enums: BTreeMap<String, ir::EnumDef>,
    /// variant name -> (enum name, tag)
    variants: HashMap<String, (String, usize)>,
    funcs: HashMap<String, ir::Signature>,
    diags: Vec<Diagnostic>,

    // Per-function mutable state.
    scopes: Vec<HashMap<String, Local>>,
    cur_ret: Type,
}

#[derive(Clone)]
struct Local {
    ty: Type,
    mutable: bool,
}

impl Checker {
    fn new() -> Self {
        Checker {
            structs: BTreeMap::new(),
            enums: BTreeMap::new(),
            variants: HashMap::new(),
            funcs: HashMap::new(),
            diags: Vec::new(),
            scopes: Vec::new(),
            cur_ret: Type::Unit,
        }
    }

    /// First pass: collect type definitions and function signatures so that
    /// definitions may refer to one another regardless of source order.
    fn collect(&mut self, program: &ast::Program) {
        // Structs and enums first (names needed to resolve types).
        for item in &program.items {
            match item {
                ast::Item::Struct(s) => {
                    if self.structs.contains_key(&s.name) || self.enums.contains_key(&s.name) {
                        self.diags.push(Diagnostic::new(
                            Stage::Type,
                            format!("duplicate type `{}`", s.name),
                            s.span,
                        ));
                        continue;
                    }
                    self.structs.insert(
                        s.name.clone(),
                        ir::StructDef {
                            name: s.name.clone(),
                            fields: vec![],
                        },
                    );
                }
                ast::Item::Enum(e) => {
                    if self.structs.contains_key(&e.name) || self.enums.contains_key(&e.name) {
                        self.diags.push(Diagnostic::new(
                            Stage::Type,
                            format!("duplicate type `{}`", e.name),
                            e.span,
                        ));
                        continue;
                    }
                    self.enums.insert(
                        e.name.clone(),
                        ir::EnumDef {
                            name: e.name.clone(),
                            variants: vec![],
                        },
                    );
                }
                _ => {}
            }
        }

        // Now resolve field/variant/payload types (all type names are known).
        for item in &program.items {
            match item {
                ast::Item::Struct(s) => {
                    let mut fields = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    for f in &s.fields {
                        if !seen.insert(f.name.clone()) {
                            self.diags.push(Diagnostic::new(
                                Stage::Type,
                                format!("duplicate field `{}` in struct `{}`", f.name, s.name),
                                f.span,
                            ));
                        }
                        match self.resolve(&f.ty, f.span) {
                            Ok(ty) => fields.push(ir::Field {
                                name: f.name.clone(),
                                ty,
                            }),
                            Err(d) => self.diags.push(d),
                        }
                    }
                    if let Some(sd) = self.structs.get_mut(&s.name) {
                        sd.fields = fields;
                    }
                }
                ast::Item::Enum(e) => {
                    let mut variants = Vec::new();
                    for (tag, v) in e.variants.iter().enumerate() {
                        if let Some((prev, _)) =
                            self.variants.insert(v.name.clone(), (e.name.clone(), tag))
                        {
                            self.diags.push(Diagnostic::new(
                                Stage::Type,
                                format!(
                                    "variant name `{}` already used (in enum `{}`); \
                                     variant names must be unique",
                                    v.name, prev
                                ),
                                v.span,
                            ));
                        }
                        let mut tys = Vec::new();
                        for t in &v.fields {
                            match self.resolve(t, v.span) {
                                Ok(ty) => tys.push(ty),
                                Err(d) => self.diags.push(d),
                            }
                        }
                        variants.push(ir::Variant {
                            name: v.name.clone(),
                            fields: tys,
                        });
                    }
                    if let Some(ed) = self.enums.get_mut(&e.name) {
                        ed.variants = variants;
                    }
                }
                _ => {}
            }
        }

        // Function signatures.
        for item in &program.items {
            if let ast::Item::Func(f) = item {
                if self.funcs.contains_key(&f.name) {
                    self.diags.push(Diagnostic::new(
                        Stage::Type,
                        format!("duplicate function `{}`", f.name),
                        f.span,
                    ));
                    continue;
                }
                let params: Vec<Type> = f
                    .params
                    .iter()
                    .filter_map(|p| self.resolve(&p.ty, p.span).ok())
                    .collect();
                let ret = match &f.ret {
                    Some(t) => self.resolve(t, f.span).unwrap_or(Type::Unit),
                    None => Type::Unit,
                };
                self.funcs
                    .insert(f.name.clone(), ir::Signature { params, ret });
            }
        }
    }

    fn resolve(&self, t: &ast::TypeExpr, span: Span) -> Result<Type, Diagnostic> {
        Ok(match t {
            ast::TypeExpr::Bool => Type::Bool,
            ast::TypeExpr::Unit => Type::Unit,
            ast::TypeExpr::Int { signed, width } => Type::Int(IntTy {
                signed: *signed,
                width: *width,
            }),
            ast::TypeExpr::Bit { width } => Type::Bit(*width),
            ast::TypeExpr::Array { elem, len } => {
                Type::Array(Box::new(self.resolve(elem, span)?), *len)
            }
            ast::TypeExpr::Named(name) => {
                if self.structs.contains_key(name) {
                    Type::Struct(name.clone())
                } else if self.enums.contains_key(name) {
                    Type::Enum(name.clone())
                } else {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("unknown type `{}`", name),
                        span,
                    ));
                }
            }
        })
    }

    // ---- functions ------------------------------------------------------

    fn check_func(&mut self, f: &ast::Func) -> Result<ir::Func, Diagnostic> {
        let mut params = Vec::new();
        self.scopes = vec![HashMap::new()];
        for p in &f.params {
            let ty = self.resolve(&p.ty, p.span)?;
            self.bind(&p.name, ty.clone(), false);
            params.push(ir::Param {
                name: p.name.clone(),
                ty,
            });
        }
        let ret = match &f.ret {
            Some(t) => self.resolve(t, f.span)?,
            None => Type::Unit,
        };
        self.cur_ret = ret.clone();

        let body = self.check_block(&f.body, Some(&ret))?;
        if body.ty != ret && !(ret == Type::Unit) {
            return Err(Diagnostic::new(
                Stage::Type,
                format!(
                    "function `{}` should return `{}` but its body has type `{}`",
                    f.name, ret, body.ty
                ),
                f.body.span,
            ));
        }
        Ok(ir::Func {
            name: f.name.clone(),
            params,
            ret,
            body,
        })
    }

    // ---- scopes ---------------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
    }
    fn bind(&mut self, name: &str, ty: Type, mutable: bool) {
        self.scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), Local { ty, mutable });
    }
    fn lookup(&self, name: &str) -> Option<&Local> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    // ---- blocks & statements -------------------------------------------

    fn check_block(&mut self, b: &ast::Block, expected: Option<&Type>) -> Result<ir::Block, Diagnostic> {
        self.push_scope();
        let result = self.check_block_inner(b, expected);
        self.pop_scope();
        result
    }

    fn check_block_inner(
        &mut self,
        b: &ast::Block,
        expected: Option<&Type>,
    ) -> Result<ir::Block, Diagnostic> {
        let mut stmts = Vec::new();
        for s in &b.stmts {
            stmts.push(self.check_stmt(s)?);
        }
        let (tail, ty) = match &b.tail {
            Some(e) => {
                let ir = self.check_expr(e, expected)?;
                let ty = ir.ty.clone();
                (Some(Box::new(ir)), ty)
            }
            None => (None, Type::Unit),
        };
        Ok(ir::Block { stmts, tail, ty })
    }

    fn check_stmt(&mut self, s: &ast::Stmt) -> Result<ir::Stmt, Diagnostic> {
        match s {
            ast::Stmt::Let {
                name,
                mutable,
                ty,
                init,
                span,
            } => {
                let expected = match ty {
                    Some(t) => Some(self.resolve(t, *span)?),
                    None => None,
                };
                let init_ir = self.check_expr(init, expected.as_ref())?;
                if let Some(t) = &expected {
                    if &init_ir.ty != t {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!(
                                "`let {}` annotated `{}` but initializer has type `{}`",
                                name, t, init_ir.ty
                            ),
                            *span,
                        ));
                    }
                }
                if init_ir.ty == Type::Unit {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("cannot bind `{}` to a value of type `()`", name),
                        *span,
                    ));
                }
                let bind_ty = init_ir.ty.clone();
                self.bind(name, bind_ty.clone(), *mutable);
                Ok(ir::Stmt::Let {
                    name: name.clone(),
                    mutable: *mutable,
                    ty: bind_ty,
                    init: init_ir,
                })
            }
            ast::Stmt::Assign { target, value, span } => {
                let place = self.check_place(target)?;
                let val = self.check_expr(value, Some(place.ty()))?;
                if &val.ty != place.ty() {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!(
                            "cannot assign value of type `{}` to place of type `{}`",
                            val.ty,
                            place.ty()
                        ),
                        *span,
                    ));
                }
                Ok(ir::Stmt::Assign { place, value: val })
            }
            ast::Stmt::While { cond, body, .. } => {
                let cond_ir = self.check_expr(cond, Some(&Type::Bool))?;
                self.expect_ty(&cond_ir.ty, &Type::Bool, cond.span(), "while condition")?;
                let body_ir = self.check_block(body, None)?;
                Ok(ir::Stmt::While {
                    cond: cond_ir,
                    body: body_ir,
                })
            }
            ast::Stmt::For {
                var,
                lo,
                hi,
                body,
                span,
            } => {
                let lo_ir = self.check_expr(lo, None)?;
                if !lo_ir.ty.is_integer() {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("for-loop bounds must be integers, found `{}`", lo_ir.ty),
                        lo.span(),
                    ));
                }
                let hi_ir = self.check_expr(hi, Some(&lo_ir.ty))?;
                if hi_ir.ty != lo_ir.ty {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!(
                            "for-loop bounds must have the same type; found `{}` and `{}`",
                            lo_ir.ty, hi_ir.ty
                        ),
                        *span,
                    ));
                }
                let ty = lo_ir.ty.clone();
                self.push_scope();
                self.bind(var, ty.clone(), false);
                let body_ir = self.check_block_inner(body, None);
                self.pop_scope();
                let body_ir = body_ir?;
                Ok(ir::Stmt::For {
                    var: var.clone(),
                    ty,
                    lo: lo_ir,
                    hi: hi_ir,
                    body: body_ir,
                })
            }
            ast::Stmt::Return { value, span } => {
                let ret = self.cur_ret.clone();
                let val = match value {
                    Some(e) => {
                        let ir = self.check_expr(e, Some(&ret))?;
                        if ir.ty != ret {
                            return Err(Diagnostic::new(
                                Stage::Type,
                                format!("returning `{}` from a function returning `{}`", ir.ty, ret),
                                *span,
                            ));
                        }
                        Some(ir)
                    }
                    None => {
                        if ret != Type::Unit {
                            return Err(Diagnostic::new(
                                Stage::Type,
                                format!("`return;` in a function returning `{}`", ret),
                                *span,
                            ));
                        }
                        None
                    }
                };
                Ok(ir::Stmt::Return { value: val })
            }
            ast::Stmt::Expr(e) => {
                let ir = self.check_expr(e, None)?;
                Ok(ir::Stmt::Expr(ir))
            }
        }
    }

    fn check_place(&mut self, e: &ast::Expr) -> Result<ir::Place, Diagnostic> {
        match e {
            ast::Expr::Ident { name, span } => {
                let local = self.lookup(name).ok_or_else(|| {
                    Diagnostic::new(Stage::Type, format!("unknown variable `{}`", name), *span)
                })?;
                if !local.mutable {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("cannot assign to immutable binding `{}` (use `let mut`)", name),
                        *span,
                    ));
                }
                Ok(ir::Place::Local {
                    name: name.clone(),
                    ty: local.ty.clone(),
                })
            }
            ast::Expr::Field { base, field, span } => {
                let base_place = self.check_place(base)?;
                let (index, ty) = self.resolve_field(base_place.ty(), field, *span)?;
                Ok(ir::Place::Field {
                    base: Box::new(base_place),
                    index,
                    ty,
                })
            }
            ast::Expr::Index { base, index, span } => {
                let base_place = self.check_place(base)?;
                let elem = match base_place.ty() {
                    Type::Array(elem, _) => (**elem).clone(),
                    other => {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!("cannot index a value of type `{}`", other),
                            *span,
                        ))
                    }
                };
                let idx = self.check_expr(index, None)?;
                if !idx.ty.is_integer() {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("array index must be an integer, found `{}`", idx.ty),
                        index.span(),
                    ));
                }
                Ok(ir::Place::Index {
                    base: Box::new(base_place),
                    index: Box::new(idx),
                    ty: elem,
                })
            }
            other => Err(Diagnostic::new(
                Stage::Type,
                "invalid assignment target",
                other.span(),
            )),
        }
    }

    fn resolve_field(
        &self,
        base_ty: &Type,
        field: &str,
        span: Span,
    ) -> Result<(usize, Type), Diagnostic> {
        match base_ty {
            Type::Struct(name) => {
                let sd = &self.structs[name];
                match sd.field_index(field) {
                    Some(i) => Ok((i, sd.fields[i].ty.clone())),
                    None => Err(Diagnostic::new(
                        Stage::Type,
                        format!("struct `{}` has no field `{}`", name, field),
                        span,
                    )),
                }
            }
            other => Err(Diagnostic::new(
                Stage::Type,
                format!("cannot access field `{}` on type `{}`", field, other),
                span,
            )),
        }
    }

    // ---- expressions ----------------------------------------------------

    fn check_expr(&mut self, e: &ast::Expr, expected: Option<&Type>) -> Result<ir::Expr, Diagnostic> {
        match e {
            ast::Expr::Int { value, span } => {
                let ty = match expected {
                    Some(t) if t.is_integer() => t.clone(),
                    _ => I32,
                };
                if !fits(*value, &ty) {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("integer literal `{}` does not fit in `{}`", value, ty),
                        *span,
                    ));
                }
                Ok(ir::Expr::new(ir::ExprKind::Int(*value), ty))
            }
            ast::Expr::Bool { value, .. } => {
                Ok(ir::Expr::new(ir::ExprKind::Bool(*value), Type::Bool))
            }
            ast::Expr::Ident { name, span } => self.check_ident(name, *span),
            ast::Expr::Unary { op, expr, span } => self.check_unary(*op, expr, *span, expected),
            ast::Expr::Binary { op, lhs, rhs, .. } => self.check_binary(*op, lhs, rhs, expected),
            ast::Expr::Call { callee, args, span } => self.check_call(callee, args, *span),
            ast::Expr::Field { base, field, span } => {
                let base_ir = self.check_expr(base, None)?;
                let (index, ty) = self.resolve_field(&base_ir.ty, field, *span)?;
                Ok(ir::Expr::new(
                    ir::ExprKind::Field {
                        base: Box::new(base_ir),
                        index,
                    },
                    ty,
                ))
            }
            ast::Expr::Index { base, index, span } => {
                let base_ir = self.check_expr(base, None)?;
                let elem = match &base_ir.ty {
                    Type::Array(elem, _) => (**elem).clone(),
                    other => {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!("cannot index a value of type `{}`", other),
                            *span,
                        ))
                    }
                };
                let idx = self.check_expr(index, None)?;
                if !idx.ty.is_integer() {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("array index must be an integer, found `{}`", idx.ty),
                        index.span(),
                    ));
                }
                Ok(ir::Expr::new(
                    ir::ExprKind::Index {
                        base: Box::new(base_ir),
                        index: Box::new(idx),
                    },
                    elem,
                ))
            }
            ast::Expr::Array { elems, span } => self.check_array(elems, *span, expected),
            ast::Expr::StructLit { name, fields, span } => {
                self.check_struct_lit(name, fields, *span)
            }
            ast::Expr::If {
                cond,
                then_branch,
                else_branch,
                span,
            } => self.check_if(cond, then_branch, else_branch.as_deref(), *span, expected),
            ast::Expr::Match {
                scrutinee, arms, span,
            } => self.check_match(scrutinee, arms, *span, expected),
            ast::Expr::Block(b) => {
                let blk = self.check_block(b, expected)?;
                let ty = blk.ty.clone();
                Ok(ir::Expr::new(ir::ExprKind::Block(blk), ty))
            }
        }
    }

    fn check_ident(&mut self, name: &str, span: Span) -> Result<ir::Expr, Diagnostic> {
        if let Some(local) = self.lookup(name) {
            return Ok(ir::Expr::new(
                ir::ExprKind::Local(name.to_string()),
                local.ty.clone(),
            ));
        }
        // A unit-like enum variant referenced by name.
        if let Some((enum_name, tag)) = self.variants.get(name).cloned() {
            let variant = &self.enums[&enum_name].variants[tag];
            if !variant.fields.is_empty() {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!(
                        "variant `{}` takes {} argument(s); call it like `{}(...)`",
                        name,
                        variant.fields.len(),
                        name
                    ),
                    span,
                ));
            }
            return Ok(ir::Expr::new(
                ir::ExprKind::MakeEnum {
                    name: enum_name.clone(),
                    tag,
                    args: vec![],
                },
                Type::Enum(enum_name),
            ));
        }
        Err(Diagnostic::new(
            Stage::Type,
            format!("unknown identifier `{}`", name),
            span,
        ))
    }

    fn check_unary(
        &mut self,
        op: UnOp,
        expr: &ast::Expr,
        span: Span,
        expected: Option<&Type>,
    ) -> Result<ir::Expr, Diagnostic> {
        match op {
            UnOp::Neg => {
                let inner = self.check_expr(expr, expected)?;
                match &inner.ty {
                    Type::Int(IntTy { signed: true, .. }) | Type::Bit(_) => {}
                    other => {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!("cannot negate a value of type `{}`", other),
                            span,
                        ))
                    }
                }
                let ty = inner.ty.clone();
                Ok(ir::Expr::new(
                    ir::ExprKind::Unary {
                        op: ir::UnOp::Neg,
                        expr: Box::new(inner),
                    },
                    ty,
                ))
            }
            UnOp::Not => {
                let inner = self.check_expr(expr, expected)?;
                match &inner.ty {
                    Type::Bool | Type::Int(_) | Type::Bit(_) => {}
                    other => {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!("cannot apply `!` to a value of type `{}`", other),
                            span,
                        ))
                    }
                }
                let ty = inner.ty.clone();
                Ok(ir::Expr::new(
                    ir::ExprKind::Unary {
                        op: ir::UnOp::Not,
                        expr: Box::new(inner),
                    },
                    ty,
                ))
            }
        }
    }

    fn check_binary(
        &mut self,
        op: BinOp,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        expected: Option<&Type>,
    ) -> Result<ir::Expr, Diagnostic> {
        let irop = lower_binop(op);
        if irop.is_logical() {
            let l = self.check_expr(lhs, Some(&Type::Bool))?;
            self.expect_ty(&l.ty, &Type::Bool, lhs.span(), "operand of `&&`/`||`")?;
            let r = self.check_expr(rhs, Some(&Type::Bool))?;
            self.expect_ty(&r.ty, &Type::Bool, rhs.span(), "operand of `&&`/`||`")?;
            return Ok(ir::Expr::new(
                ir::ExprKind::Binary {
                    op: irop,
                    lhs: Box::new(l),
                    rhs: Box::new(r),
                },
                Type::Bool,
            ));
        }

        // Shifts: lhs determines result type; rhs is an independent integer.
        if matches!(irop, ir::BinOp::Shl | ir::BinOp::Shr) {
            let l = self.check_expr(lhs, expected)?;
            if !l.ty.is_integer() {
                return Err(self.op_type_err(op, &l.ty, lhs.span()));
            }
            let r = self.check_expr(rhs, None)?;
            if !r.ty.is_integer() {
                return Err(self.op_type_err(op, &r.ty, rhs.span()));
            }
            let ty = l.ty.clone();
            return Ok(ir::Expr::new(
                ir::ExprKind::Binary {
                    op: irop,
                    lhs: Box::new(l),
                    rhs: Box::new(r),
                },
                ty,
            ));
        }

        // Comparisons and arithmetic/bitwise unify their two operands.
        let unify_expected = if irop.is_comparison() { None } else { expected };
        let (l, r, common) = self.unify_operands(lhs, rhs, unify_expected)?;

        if irop.is_comparison() {
            let ordering = matches!(
                irop,
                ir::BinOp::Lt | ir::BinOp::Le | ir::BinOp::Gt | ir::BinOp::Ge
            );
            if ordering && !common.is_integer() {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!("cannot order values of type `{}`", common),
                    lhs.span().merge(rhs.span()),
                ));
            }
            if !ordering && !(common.is_integer() || common == Type::Bool) {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!("cannot compare values of type `{}`", common),
                    lhs.span().merge(rhs.span()),
                ));
            }
            return Ok(ir::Expr::new(
                ir::ExprKind::Binary {
                    op: irop,
                    lhs: Box::new(l),
                    rhs: Box::new(r),
                },
                Type::Bool,
            ));
        }

        // Arithmetic / bitwise.
        if !common.is_integer() {
            return Err(self.op_type_err(op, &common, lhs.span().merge(rhs.span())));
        }
        Ok(ir::Expr::new(
            ir::ExprKind::Binary {
                op: irop,
                lhs: Box::new(l),
                rhs: Box::new(r),
            },
            common,
        ))
    }

    /// Check two operands so they share a common type, inferring a literal's
    /// type from its non-literal partner (or from `expected`, or `i32`).
    fn unify_operands(
        &mut self,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        expected: Option<&Type>,
    ) -> Result<(ir::Expr, ir::Expr, Type), Diagnostic> {
        let target: Type = if let Some(e) = expected.filter(|t| t.is_integer()) {
            e.clone()
        } else if !is_int_literalish(lhs) {
            self.check_expr(lhs, None)?.ty
        } else if !is_int_literalish(rhs) {
            self.check_expr(rhs, None)?.ty
        } else {
            I32
        };

        let l = self.check_expr(lhs, Some(&target))?;
        let r = self.check_expr(rhs, Some(&target))?;
        if l.ty != r.ty {
            return Err(Diagnostic::new(
                Stage::Type,
                format!("mismatched types: `{}` and `{}`", l.ty, r.ty),
                lhs.span().merge(rhs.span()),
            ));
        }
        let ty = l.ty.clone();
        Ok((l, r, ty))
    }

    fn check_call(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::Expr],
        span: Span,
    ) -> Result<ir::Expr, Diagnostic> {
        let name = match callee {
            ast::Expr::Ident { name, .. } => name.clone(),
            other => {
                return Err(Diagnostic::new(
                    Stage::Type,
                    "only named functions and enum variants can be called",
                    other.span(),
                ))
            }
        };

        // Built-in `print`.
        if name == "print" {
            if args.len() != 1 {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!("`print` takes exactly 1 argument, found {}", args.len()),
                    span,
                ));
            }
            let arg = self.check_expr(&args[0], None)?;
            if arg.ty == Type::Unit {
                return Err(Diagnostic::new(
                    Stage::Type,
                    "cannot print a value of type `()`",
                    args[0].span(),
                ));
            }
            return Ok(ir::Expr::new(
                ir::ExprKind::Print { arg: Box::new(arg) },
                Type::Unit,
            ));
        }

        // Enum variant constructor.
        if let Some((enum_name, tag)) = self.variants.get(&name).cloned() {
            let field_tys = self.enums[&enum_name].variants[tag].fields.clone();
            if args.len() != field_tys.len() {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!(
                        "variant `{}` takes {} argument(s), found {}",
                        name,
                        field_tys.len(),
                        args.len()
                    ),
                    span,
                ));
            }
            let mut arg_irs = Vec::new();
            for (a, t) in args.iter().zip(field_tys.iter()) {
                let ir = self.check_expr(a, Some(t))?;
                if &ir.ty != t {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("expected `{}` for variant payload, found `{}`", t, ir.ty),
                        a.span(),
                    ));
                }
                arg_irs.push(ir);
            }
            return Ok(ir::Expr::new(
                ir::ExprKind::MakeEnum {
                    name: enum_name.clone(),
                    tag,
                    args: arg_irs,
                },
                Type::Enum(enum_name),
            ));
        }

        // Ordinary function call.
        let sig = self.funcs.get(&name).cloned().ok_or_else(|| {
            Diagnostic::new(Stage::Type, format!("unknown function `{}`", name), span)
        })?;
        if args.len() != sig.params.len() {
            return Err(Diagnostic::new(
                Stage::Type,
                format!(
                    "function `{}` takes {} argument(s), found {}",
                    name,
                    sig.params.len(),
                    args.len()
                ),
                span,
            ));
        }
        let mut arg_irs = Vec::new();
        for (a, t) in args.iter().zip(sig.params.iter()) {
            let ir = self.check_expr(a, Some(t))?;
            if &ir.ty != t {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!("expected `{}` for argument, found `{}`", t, ir.ty),
                    a.span(),
                ));
            }
            arg_irs.push(ir);
        }
        Ok(ir::Expr::new(
            ir::ExprKind::Call {
                func: name,
                args: arg_irs,
            },
            sig.ret,
        ))
    }

    fn check_array(
        &mut self,
        elems: &[ast::Expr],
        span: Span,
        expected: Option<&Type>,
    ) -> Result<ir::Expr, Diagnostic> {
        let elem_expected = match expected {
            Some(Type::Array(t, _)) => Some((**t).clone()),
            _ => None,
        };
        if elems.is_empty() {
            // Need an annotation to know the element type.
            return match expected {
                Some(Type::Array(t, 0)) => Ok(ir::Expr::new(
                    ir::ExprKind::Array(vec![]),
                    Type::Array(t.clone(), 0),
                )),
                _ => Err(Diagnostic::new(
                    Stage::Type,
                    "cannot infer the type of an empty array literal; add a type annotation",
                    span,
                )),
            };
        }
        let mut irs = Vec::new();
        let mut elem_ty: Option<Type> = elem_expected.clone();
        for e in elems {
            let ir = self.check_expr(e, elem_ty.as_ref())?;
            match &elem_ty {
                Some(t) if t != &ir.ty => {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!(
                            "array elements must share a type; found `{}` and `{}`",
                            t, ir.ty
                        ),
                        e.span(),
                    ));
                }
                None => elem_ty = Some(ir.ty.clone()),
                _ => {}
            }
            irs.push(ir);
        }
        let ty = Type::Array(Box::new(elem_ty.unwrap()), irs.len() as u64);
        Ok(ir::Expr::new(ir::ExprKind::Array(irs), ty))
    }

    fn check_struct_lit(
        &mut self,
        name: &str,
        fields: &[(String, ast::Expr)],
        span: Span,
    ) -> Result<ir::Expr, Diagnostic> {
        let sd = self
            .structs
            .get(name)
            .cloned()
            .ok_or_else(|| Diagnostic::new(Stage::Type, format!("unknown struct `{}`", name), span))?;

        let mut provided: HashMap<&str, &ast::Expr> = HashMap::new();
        for (fname, fexpr) in fields {
            if provided.insert(fname.as_str(), fexpr).is_some() {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!("field `{}` specified more than once", fname),
                    fexpr.span(),
                ));
            }
        }

        let mut field_irs = Vec::new();
        for f in &sd.fields {
            match provided.remove(f.name.as_str()) {
                Some(fexpr) => {
                    let ir = self.check_expr(fexpr, Some(&f.ty))?;
                    if ir.ty != f.ty {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!(
                                "field `{}` of `{}` expects `{}`, found `{}`",
                                f.name, name, f.ty, ir.ty
                            ),
                            fexpr.span(),
                        ));
                    }
                    field_irs.push(ir);
                }
                None => {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("missing field `{}` in `{}` literal", f.name, name),
                        span,
                    ));
                }
            }
        }
        if let Some((extra, e)) = provided.iter().next() {
            return Err(Diagnostic::new(
                Stage::Type,
                format!("struct `{}` has no field `{}`", name, extra),
                e.span(),
            ));
        }

        Ok(ir::Expr::new(
            ir::ExprKind::MakeStruct {
                name: name.to_string(),
                fields: field_irs,
            },
            Type::Struct(name.to_string()),
        ))
    }

    fn check_if(
        &mut self,
        cond: &ast::Expr,
        then_branch: &ast::Block,
        else_branch: Option<&ast::ElseBranch>,
        span: Span,
        expected: Option<&Type>,
    ) -> Result<ir::Expr, Diagnostic> {
        let cond_ir = self.check_expr(cond, Some(&Type::Bool))?;
        self.expect_ty(&cond_ir.ty, &Type::Bool, cond.span(), "if condition")?;

        match else_branch {
            Some(eb) => {
                let then_ir = self.check_block(then_branch, expected)?;
                let else_ir = match eb {
                    ast::ElseBranch::Block(b) => self.check_block(b, expected)?,
                    ast::ElseBranch::If(inner) => {
                        // Wrap the chained `else if` as a single-expression block.
                        let e = self.check_expr(inner, expected)?;
                        let ty = e.ty.clone();
                        ir::Block {
                            stmts: vec![],
                            tail: Some(Box::new(e)),
                            ty,
                        }
                    }
                };
                if then_ir.ty != else_ir.ty {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!(
                            "`if` and `else` branches have different types: `{}` vs `{}`",
                            then_ir.ty, else_ir.ty
                        ),
                        span,
                    ));
                }
                let ty = then_ir.ty.clone();
                Ok(ir::Expr::new(
                    ir::ExprKind::If {
                        cond: Box::new(cond_ir),
                        then_branch: then_ir,
                        else_branch: else_ir,
                    },
                    ty,
                ))
            }
            None => {
                // No else: the `if` is a statement-like unit expression.
                let then_ir = self.check_block(then_branch, Some(&Type::Unit))?;
                if then_ir.ty != Type::Unit {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!(
                            "`if` without `else` must have type `()`, found `{}`",
                            then_ir.ty
                        ),
                        span,
                    ));
                }
                let else_ir = ir::Block {
                    stmts: vec![],
                    tail: None,
                    ty: Type::Unit,
                };
                Ok(ir::Expr::new(
                    ir::ExprKind::If {
                        cond: Box::new(cond_ir),
                        then_branch: then_ir,
                        else_branch: else_ir,
                    },
                    Type::Unit,
                ))
            }
        }
    }

    fn check_match(
        &mut self,
        scrutinee: &ast::Expr,
        arms: &[ast::MatchArm],
        span: Span,
        expected: Option<&Type>,
    ) -> Result<ir::Expr, Diagnostic> {
        let scrut = self.check_expr(scrutinee, None)?;
        let scrut_ty = scrut.ty.clone();

        if arms.is_empty() {
            return Err(Diagnostic::new(
                Stage::Type,
                "`match` must have at least one arm",
                span,
            ));
        }

        let mut ir_arms = Vec::new();
        let mut result_ty: Option<Type> = expected.cloned();
        for arm in arms {
            self.push_scope();
            let pat_result = self
                .check_pattern(&arm.pattern, &scrut_ty)
                .and_then(|pat| {
                    let body = self.check_expr(&arm.body, result_ty.as_ref())?;
                    Ok((pat, body))
                });
            self.pop_scope();
            let (pat, body) = pat_result?;

            match &result_ty {
                Some(t) if t != &body.ty => {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!(
                            "match arms have different types: `{}` vs `{}`",
                            t, body.ty
                        ),
                        arm.span,
                    ));
                }
                None => result_ty = Some(body.ty.clone()),
                _ => {}
            }
            ir_arms.push(ir::Arm { pattern: pat, body });
        }

        self.check_exhaustive(&scrut_ty, arms, span)?;

        let ty = result_ty.unwrap_or(Type::Unit);
        Ok(ir::Expr::new(
            ir::ExprKind::Match {
                scrutinee: Box::new(scrut),
                arms: ir_arms,
            },
            ty,
        ))
    }

    fn check_pattern(&mut self, p: &ast::Pattern, ty: &Type) -> Result<ir::Pattern, Diagnostic> {
        match p {
            ast::Pattern::Wildcard { .. } => Ok(ir::Pattern::Wildcard),
            ast::Pattern::Binding { name, .. } => {
                self.bind(name, ty.clone(), false);
                Ok(ir::Pattern::Binding {
                    name: name.clone(),
                    ty: ty.clone(),
                })
            }
            ast::Pattern::Int { value, span } => {
                if !ty.is_integer() {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("integer pattern cannot match a value of type `{}`", ty),
                        *span,
                    ));
                }
                if !fits(*value, ty) {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("pattern `{}` does not fit in `{}`", value, ty),
                        *span,
                    ));
                }
                Ok(ir::Pattern::Int(*value))
            }
            ast::Pattern::Bool { value, span } => {
                if ty != &Type::Bool {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("boolean pattern cannot match a value of type `{}`", ty),
                        *span,
                    ));
                }
                Ok(ir::Pattern::Bool(*value))
            }
            ast::Pattern::Variant {
                name,
                subpatterns,
                span,
            } => {
                let enum_name = match ty {
                    Type::Enum(n) => n.clone(),
                    other => {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!("variant pattern `{}` cannot match type `{}`", name, other),
                            *span,
                        ))
                    }
                };
                let (ven, tag) = self.variants.get(name).cloned().ok_or_else(|| {
                    Diagnostic::new(Stage::Type, format!("unknown variant `{}`", name), *span)
                })?;
                if ven != enum_name {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("variant `{}` belongs to enum `{}`, not `{}`", name, ven, enum_name),
                        *span,
                    ));
                }
                let field_tys = self.enums[&enum_name].variants[tag].fields.clone();
                if subpatterns.len() != field_tys.len() {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!(
                            "variant `{}` has {} field(s) but pattern binds {}",
                            name,
                            field_tys.len(),
                            subpatterns.len()
                        ),
                        *span,
                    ));
                }
                let mut subs = Vec::new();
                for (sp, ft) in subpatterns.iter().zip(field_tys.iter()) {
                    subs.push(self.check_pattern(sp, ft)?);
                }
                Ok(ir::Pattern::Variant {
                    enum_name,
                    tag,
                    subpatterns: subs,
                })
            }
        }
    }

    /// Exhaustiveness check. A `_` or top-level binding always succeeds. For
    /// enums, every variant must be covered by an *irrefutable* variant pattern
    /// (one whose subpatterns are themselves irrefutable). For `bool`, both
    /// `true` and `false` must appear. Integers and other types require a
    /// wildcard/binding.
    fn check_exhaustive(
        &self,
        ty: &Type,
        arms: &[ast::MatchArm],
        span: Span,
    ) -> Result<(), Diagnostic> {
        let has_catch_all = arms.iter().any(|a| {
            matches!(
                a.pattern,
                ast::Pattern::Wildcard { .. } | ast::Pattern::Binding { .. }
            )
        });
        if has_catch_all {
            return Ok(());
        }

        match ty {
            Type::Enum(name) => {
                let ed = &self.enums[name];
                let mut covered = vec![false; ed.variants.len()];
                for a in arms {
                    if let ast::Pattern::Variant {
                        name: vname,
                        subpatterns,
                        ..
                    } = &a.pattern
                    {
                        if subpatterns.iter().all(is_irrefutable) {
                            if let Some(tag) = ed.variant_tag(vname) {
                                covered[tag] = true;
                            }
                        }
                    }
                }
                let missing: Vec<&str> = ed
                    .variants
                    .iter()
                    .zip(covered.iter())
                    .filter(|(_, c)| !**c)
                    .map(|(v, _)| v.name.as_str())
                    .collect();
                if missing.is_empty() {
                    Ok(())
                } else {
                    Err(Diagnostic::new(
                        Stage::Type,
                        format!(
                            "non-exhaustive match on `{}`: variant(s) {} not covered",
                            name,
                            missing.join(", ")
                        ),
                        span,
                    ))
                }
            }
            Type::Bool => {
                let mut t = false;
                let mut f = false;
                for a in arms {
                    if let ast::Pattern::Bool { value, .. } = a.pattern {
                        if value {
                            t = true;
                        } else {
                            f = true;
                        }
                    }
                }
                if t && f {
                    Ok(())
                } else {
                    Err(Diagnostic::new(
                        Stage::Type,
                        "non-exhaustive match on `bool`: add the missing case or a `_` arm",
                        span,
                    ))
                }
            }
            other => Err(Diagnostic::new(
                Stage::Type,
                format!(
                    "non-exhaustive match on `{}`: add a `_` (wildcard) arm",
                    other
                ),
                span,
            )),
        }
    }

    // ---- helpers --------------------------------------------------------

    fn expect_ty(&self, got: &Type, want: &Type, span: Span, ctx: &str) -> Result<(), Diagnostic> {
        if got == want {
            Ok(())
        } else {
            Err(Diagnostic::new(
                Stage::Type,
                format!("{} must be `{}`, found `{}`", ctx, want, got),
                span,
            ))
        }
    }

    fn op_type_err(&self, op: BinOp, ty: &Type, span: Span) -> Diagnostic {
        Diagnostic::new(
            Stage::Type,
            format!("operator `{:?}` is not defined for type `{}`", op, ty),
            span,
        )
    }
}

/// A pattern is irrefutable if it always matches: wildcard or binding, or a
/// variant whose subpatterns are all irrefutable.
fn is_irrefutable(p: &ast::Pattern) -> bool {
    matches!(
        p,
        ast::Pattern::Wildcard { .. } | ast::Pattern::Binding { .. }
    )
}

/// Syntactic test for "a bare integer literal" (possibly negated), used to
/// decide which side of a binary op drives literal type inference.
fn is_int_literalish(e: &ast::Expr) -> bool {
    match e {
        ast::Expr::Int { .. } => true,
        ast::Expr::Unary {
            op: UnOp::Neg,
            expr,
            ..
        } => is_int_literalish(expr),
        _ => false,
    }
}

fn lower_binop(op: BinOp) -> ir::BinOp {
    match op {
        BinOp::Add => ir::BinOp::Add,
        BinOp::Sub => ir::BinOp::Sub,
        BinOp::Mul => ir::BinOp::Mul,
        BinOp::Div => ir::BinOp::Div,
        BinOp::Rem => ir::BinOp::Rem,
        BinOp::And => ir::BinOp::And,
        BinOp::Or => ir::BinOp::Or,
        BinOp::BitAnd => ir::BinOp::BitAnd,
        BinOp::BitOr => ir::BinOp::BitOr,
        BinOp::BitXor => ir::BinOp::BitXor,
        BinOp::Shl => ir::BinOp::Shl,
        BinOp::Shr => ir::BinOp::Shr,
        BinOp::Eq => ir::BinOp::Eq,
        BinOp::Ne => ir::BinOp::Ne,
        BinOp::Lt => ir::BinOp::Lt,
        BinOp::Le => ir::BinOp::Le,
        BinOp::Gt => ir::BinOp::Gt,
        BinOp::Ge => ir::BinOp::Ge,
    }
}

/// Does integer literal `v` fit in integer-like type `ty`?
fn fits(v: i128, ty: &Type) -> bool {
    match ty {
        Type::Int(IntTy { signed, width }) => {
            let w = *width;
            if *signed {
                let max = (1i128 << (w - 1)) - 1;
                let min = -(1i128 << (w - 1));
                v >= min && v <= max
            } else {
                if v < 0 {
                    return false;
                }
                if w >= 128 {
                    true
                } else {
                    v <= (1i128 << w) - 1
                }
            }
        }
        Type::Bit(n) => {
            if v < 0 {
                return false;
            }
            if *n >= 127 {
                true
            } else {
                v <= (1i128 << n) - 1
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    fn ck(src: &str) -> Result<ir::Module, Vec<Diagnostic>> {
        check(&parse(&lex(src).unwrap()).unwrap())
    }

    fn err_msg(src: &str) -> String {
        ck(src).unwrap_err().remove(0).message
    }

    #[test]
    fn milestone_typechecks() {
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
        let m = ck(src).unwrap();
        assert_eq!(m.funcs.len(), 3);
        assert_eq!(m.funcs["add8"].ret, Type::Bit(8));
    }

    #[test]
    fn literal_out_of_range_for_bit() {
        let msg = err_msg("fn f() -> bit<8> { 300 }");
        assert!(msg.contains("does not fit"), "{}", msg);
    }

    #[test]
    fn mismatched_int_widths_error() {
        let msg = err_msg("fn f(a: u8, b: u32) -> u8 { a + b }");
        assert!(msg.contains("mismatched"), "{}", msg);
    }

    #[test]
    fn non_exhaustive_match_errors() {
        let src = r#"
            enum E { A, B }
            fn f(e: E) -> u8 { match e { A => 1 } }
        "#;
        let msg = err_msg(src);
        assert!(msg.contains("non-exhaustive"), "{}", msg);
    }

    #[test]
    fn exhaustive_with_wildcard_ok() {
        let src = r#"
            enum E { A, B, C }
            fn f(e: E) -> u8 { match e { A => 1, _ => 0 } }
        "#;
        assert!(ck(src).is_ok());
    }

    #[test]
    fn assign_immutable_errors() {
        let msg = err_msg("fn f() -> u8 { let x = 1; x = 2; x }");
        assert!(msg.contains("immutable"), "{}", msg);
    }

    #[test]
    fn unknown_type_errors() {
        let msg = err_msg("fn f(x: Nope) -> u8 { 0 }");
        assert!(msg.contains("unknown type"), "{}", msg);
    }

    #[test]
    fn condition_must_be_bool() {
        let msg = err_msg("fn f() -> u8 { if 1 { 2 } else { 3 } }");
        assert!(msg.contains("if condition"), "{}", msg);
    }

    #[test]
    fn literal_inference_through_struct_and_let() {
        let src = r#"
            struct P { x: u16, y: u16 }
            fn f() -> u16 { let p = P { x: 1, y: 2 }; p.x + p.y }
        "#;
        let m = ck(src).unwrap();
        assert_eq!(m.funcs["f"].ret, Type::Int(IntTy { signed: false, width: 16 }));
    }

    #[test]
    fn for_loop_and_array_typecheck() {
        let src = r#"
            fn sum() -> u32 {
                let a: [u32; 3] = [10, 20, 30];
                let mut s: u32 = 0;
                for i in 0..3 { s = s + a[i]; }
                s
            }
        "#;
        assert!(ck(src).is_ok());
    }
}

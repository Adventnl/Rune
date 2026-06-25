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
///
/// Definitions can be nested in modules (`mod`). Every item is keyed in the IR
/// by its **fully-qualified name** (e.g. `math::clamp`, `geom::Point`); items at
/// the program root keep their bare name, so single-file programs are
/// unaffected. Names are resolved relative to the enclosing module, honouring
/// `use` imports.
pub fn check(program: &ast::Program) -> Result<ir::Module, Vec<Diagnostic>> {
    let mut cx = Checker::new();
    let root: Vec<String> = vec![];
    cx.register_items(&program.items, &root);
    cx.resolve_defs(&program.items, &root);
    if !cx.diags.is_empty() {
        return Err(cx.diags);
    }

    let mut module = ir::Module::default();
    module.structs = cx.structs.clone();
    module.enums = cx.enums.clone();
    cx.check_bodies(&program.items, &root, &mut module);
    module.consts = cx.consts.clone();

    if cx.diags.is_empty() {
        Ok(module)
    } else {
        Err(cx.diags)
    }
}

/// The three kinds of named, qualifiable items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    Struct,
    Enum,
    Func,
    Const,
}

struct Checker {
    structs: BTreeMap<String, ir::StructDef>,
    enums: BTreeMap<String, ir::EnumDef>,
    funcs: HashMap<String, ir::Signature>,
    consts: BTreeMap<String, ir::ConstDef>,
    /// Fully-qualified item name -> what kind of item it is.
    kinds: HashMap<String, ItemKind>,
    /// Set of fully-qualified module names that exist.
    modules: std::collections::HashSet<String>,
    /// module key (`::`-joined, "" for root) -> `use` aliases: (simple name -> target path).
    uses: HashMap<String, Vec<(String, Vec<String>)>>,
    /// module key -> glob-imported module paths (`use m::*;`).
    globs: HashMap<String, Vec<Vec<String>>>,
    diags: Vec<Diagnostic>,

    // Per-function mutable state.
    scopes: Vec<HashMap<String, Local>>,
    cur_mod: Vec<String>,
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
            funcs: HashMap::new(),
            consts: BTreeMap::new(),
            kinds: HashMap::new(),
            modules: std::collections::HashSet::new(),
            uses: HashMap::new(),
            globs: HashMap::new(),
            diags: Vec::new(),
            scopes: Vec::new(),
            cur_mod: Vec::new(),
            cur_ret: Type::Unit,
        }
    }

    // ---- collection passes (module-aware) ------------------------------

    /// Pass 1: register every item's fully-qualified name and kind, every
    /// module, and every `use` import, recursing into modules.
    fn register_items(&mut self, items: &[ast::Item], cur: &[String]) {
        for item in items {
            match item {
                ast::Item::Struct(s) => self.register_named(&s.name, ItemKind::Struct, cur, s.span),
                ast::Item::Enum(e) => self.register_named(&e.name, ItemKind::Enum, cur, e.span),
                ast::Item::Func(f) => self.register_named(&f.name, ItemKind::Func, cur, f.span),
                ast::Item::Const(c) => self.register_named(&c.name, ItemKind::Const, cur, c.span),
                ast::Item::Mod(m) => {
                    let mfqn = fqn(cur, &m.name);
                    self.modules.insert(mfqn);
                    if let Some(inner) = &m.items {
                        let mut child = cur.to_vec();
                        child.push(m.name.clone());
                        self.register_items(inner, &child);
                    }
                }
                ast::Item::Use(u) => {
                    let key = cur.join("::");
                    if u.glob {
                        self.globs.entry(key).or_default().push(u.path.clone());
                    } else if let Some(alias) = u.path.last() {
                        self.uses
                            .entry(key)
                            .or_default()
                            .push((alias.clone(), u.path.clone()));
                    }
                }
            }
        }
    }

    fn register_named(&mut self, name: &str, kind: ItemKind, cur: &[String], span: Span) {
        let f = fqn(cur, name);
        if self.kinds.contains_key(&f) {
            self.diags.push(Diagnostic::new(
                Stage::Type,
                format!("duplicate definition `{}`", f),
                span,
            ));
            return;
        }
        self.kinds.insert(f.clone(), kind);
        match kind {
            ItemKind::Struct => {
                self.structs.insert(
                    f.clone(),
                    ir::StructDef {
                        name: f,
                        fields: vec![],
                    },
                );
            }
            ItemKind::Enum => {
                self.enums.insert(
                    f.clone(),
                    ir::EnumDef {
                        name: f,
                        variants: vec![],
                    },
                );
            }
            ItemKind::Func | ItemKind::Const => {}
        }
    }

    /// Pass 2: resolve struct fields, enum payloads, and function signatures now
    /// that every item name is known.
    fn resolve_defs(&mut self, items: &[ast::Item], cur: &[String]) {
        for item in items {
            match item {
                ast::Item::Struct(s) => {
                    let f = fqn(cur, &s.name);
                    let mut fields = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    for fd in &s.fields {
                        if !seen.insert(fd.name.clone()) {
                            self.diags.push(Diagnostic::new(
                                Stage::Type,
                                format!("duplicate field `{}` in struct `{}`", fd.name, f),
                                fd.span,
                            ));
                        }
                        match self.resolve_ty(&fd.ty, cur, fd.span) {
                            Ok(ty) => fields.push(ir::Field {
                                name: fd.name.clone(),
                                ty,
                            }),
                            Err(d) => self.diags.push(d),
                        }
                    }
                    if let Some(sd) = self.structs.get_mut(&f) {
                        sd.fields = fields;
                    }
                }
                ast::Item::Enum(e) => {
                    let f = fqn(cur, &e.name);
                    let mut variants = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    for v in &e.variants {
                        if !seen.insert(v.name.clone()) {
                            self.diags.push(Diagnostic::new(
                                Stage::Type,
                                format!("duplicate variant `{}` in enum `{}`", v.name, f),
                                v.span,
                            ));
                        }
                        let mut tys = Vec::new();
                        for t in &v.fields {
                            match self.resolve_ty(t, cur, v.span) {
                                Ok(ty) => tys.push(ty),
                                Err(d) => self.diags.push(d),
                            }
                        }
                        variants.push(ir::Variant {
                            name: v.name.clone(),
                            fields: tys,
                        });
                    }
                    if let Some(ed) = self.enums.get_mut(&f) {
                        ed.variants = variants;
                    }
                }
                ast::Item::Func(fdef) => {
                    let f = fqn(cur, &fdef.name);
                    let params: Vec<Type> = fdef
                        .params
                        .iter()
                        .filter_map(|p| self.resolve_ty(&p.ty, cur, p.span).ok())
                        .collect();
                    let ret = match &fdef.ret {
                        Some(t) => self.resolve_ty(t, cur, fdef.span).unwrap_or(Type::Unit),
                        None => Type::Unit,
                    };
                    self.funcs.insert(f, ir::Signature { params, ret });
                }
                ast::Item::Const(c) => {
                    let f = fqn(cur, &c.name);
                    let ty = self
                        .resolve_ty(&c.ty, cur, c.span)
                        .unwrap_or(Type::Unit);
                    // The value is checked in pass 3 (when all signatures exist);
                    // store the type now so references resolve.
                    self.consts.insert(
                        f.clone(),
                        ir::ConstDef {
                            name: f,
                            ty,
                            init: ir::Expr::new(ir::ExprKind::Unit, Type::Unit),
                        },
                    );
                }
                ast::Item::Mod(m) => {
                    if let Some(inner) = &m.items {
                        let mut child = cur.to_vec();
                        child.push(m.name.clone());
                        self.resolve_defs(inner, &child);
                    }
                }
                ast::Item::Use(_) => {}
            }
        }
    }

    /// Pass 3: typecheck function bodies, producing IR keyed by fully-qualified
    /// name.
    fn check_bodies(&mut self, items: &[ast::Item], cur: &[String], module: &mut ir::Module) {
        for item in items {
            match item {
                ast::Item::Func(fdef) => {
                    self.cur_mod = cur.to_vec();
                    match self.check_func(fdef, cur) {
                        Ok(func) => {
                            module.funcs.insert(func.name.clone(), func);
                        }
                        Err(d) => self.diags.push(d),
                    }
                }
                ast::Item::Const(c) => {
                    self.cur_mod = cur.to_vec();
                    self.scopes = vec![HashMap::new()];
                    let f = fqn(cur, &c.name);
                    let ty = self.consts[&f].ty.clone();
                    match self.check_expr(&c.value, Some(&ty)) {
                        Ok(init) if init.ty == ty => {
                            if let Some(cd) = self.consts.get_mut(&f) {
                                cd.init = init;
                            }
                        }
                        Ok(init) => self.diags.push(Diagnostic::new(
                            Stage::Type,
                            format!(
                                "const `{}` declared `{}` but its value has type `{}`",
                                f, ty, init.ty
                            ),
                            c.span,
                        )),
                        Err(d) => self.diags.push(d),
                    }
                }
                ast::Item::Mod(m) => {
                    if let Some(inner) = &m.items {
                        let mut child = cur.to_vec();
                        child.push(m.name.clone());
                        self.check_bodies(inner, &child, module);
                    }
                }
                _ => {}
            }
        }
    }

    // ---- name resolution ------------------------------------------------

    /// Resolve a (possibly module-qualified) item path from module `cur`.
    fn resolve_item(&self, cur: &[String], path: &[String]) -> Option<(String, ItemKind)> {
        if path.is_empty() {
            return None;
        }
        // 1. Lexical: try the path relative to `cur` and each ancestor module.
        for k in (0..=cur.len()).rev() {
            let mut segs = cur[..k].to_vec();
            segs.extend_from_slice(path);
            let key = segs.join("::");
            if let Some(kind) = self.kinds.get(&key) {
                return Some((key, *kind));
            }
        }
        // 2. `use` aliases visible in `cur` (or an ancestor module).
        for k in (0..=cur.len()).rev() {
            let modk = cur[..k].join("::");
            if let Some(list) = self.uses.get(&modk) {
                if let Some((_, target)) = list.iter().find(|(a, _)| a == &path[0]) {
                    let mut segs = target.clone();
                    segs.extend_from_slice(&path[1..]);
                    let key = segs.join("::");
                    if let Some(kind) = self.kinds.get(&key) {
                        return Some((key, *kind));
                    }
                }
            }
        }
        // 3. Glob imports (single-segment names only).
        if path.len() == 1 {
            for k in (0..=cur.len()).rev() {
                let modk = cur[..k].join("::");
                if let Some(list) = self.globs.get(&modk) {
                    for g in list {
                        let mut segs = g.clone();
                        segs.push(path[0].clone());
                        let key = segs.join("::");
                        if let Some(kind) = self.kinds.get(&key) {
                            return Some((key, *kind));
                        }
                    }
                }
            }
        }
        None
    }

    /// Resolve an enum variant reference (qualified `Enum::Variant` or bare
    /// `Variant`) to `(enum fqn, tag)`.
    fn resolve_variant(&self, cur: &[String], path: &[String]) -> Option<(String, usize)> {
        if path.len() >= 2 {
            let (enum_path, last) = path.split_at(path.len() - 1);
            if let Some((efqn, ItemKind::Enum)) = self.resolve_item(cur, enum_path) {
                if let Some(tag) = self.enums[&efqn].variant_tag(&last[0]) {
                    return Some((efqn, tag));
                }
            }
            return None;
        }
        // Bare variant: find the unique in-scope enum that declares it.
        let v = &path[0];
        let mut hit: Option<(String, usize)> = None;
        let mut count = 0;
        for (efqn, ed) in &self.enums {
            if let Some(tag) = ed.variant_tag(v) {
                if self.enum_in_scope(cur, efqn) {
                    hit = Some((efqn.clone(), tag));
                    count += 1;
                }
            }
        }
        if count == 1 {
            hit
        } else {
            None
        }
    }

    /// Is enum `efqn` reachable by its simple name from module `cur` (directly,
    /// or via a glob import of its module)?
    fn enum_in_scope(&self, cur: &[String], efqn: &str) -> bool {
        let simple = efqn.rsplit("::").next().unwrap().to_string();
        if let Some((f, ItemKind::Enum)) = self.resolve_item(cur, &[simple]) {
            if f == efqn {
                return true;
            }
        }
        let parent: Vec<String> = {
            let mut segs: Vec<String> = efqn.split("::").map(|s| s.to_string()).collect();
            segs.pop();
            segs
        };
        for k in (0..=cur.len()).rev() {
            let modk = cur[..k].join("::");
            if let Some(list) = self.globs.get(&modk) {
                if list.iter().any(|g| g == &parent) {
                    return true;
                }
            }
        }
        false
    }

    fn resolve_ty(&self, t: &ast::TypeExpr, cur: &[String], span: Span) -> Result<Type, Diagnostic> {
        Ok(match t {
            ast::TypeExpr::Bool => Type::Bool,
            ast::TypeExpr::Unit => Type::Unit,
            ast::TypeExpr::Int { signed, width } => Type::Int(IntTy {
                signed: *signed,
                width: *width,
            }),
            ast::TypeExpr::Bit { width } => Type::Bit(*width),
            ast::TypeExpr::Array { elem, len } => {
                Type::Array(Box::new(self.resolve_ty(elem, cur, span)?), *len)
            }
            ast::TypeExpr::Tuple(elems) => {
                let mut ts = Vec::new();
                for e in elems {
                    ts.push(self.resolve_ty(e, cur, span)?);
                }
                Type::Tuple(ts)
            }
            ast::TypeExpr::Named(path) => match self.resolve_item(cur, path) {
                Some((f, ItemKind::Struct)) => Type::Struct(f),
                Some((f, ItemKind::Enum)) => Type::Enum(f),
                Some((f, ItemKind::Func)) => {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("`{}` is a function, not a type", f),
                        span,
                    ))
                }
                Some((f, ItemKind::Const)) => {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("`{}` is a constant, not a type", f),
                        span,
                    ))
                }
                None => {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("unknown type `{}`", path.join("::")),
                        span,
                    ))
                }
            },
        })
    }

    // ---- functions ------------------------------------------------------

    fn check_func(&mut self, f: &ast::Func, cur: &[String]) -> Result<ir::Func, Diagnostic> {
        let mut params = Vec::new();
        self.scopes = vec![HashMap::new()];
        for p in &f.params {
            let ty = self.resolve_ty(&p.ty, cur, p.span)?;
            self.bind(&p.name, ty.clone(), false);
            params.push(ir::Param {
                name: p.name.clone(),
                ty,
            });
        }
        let ret = match &f.ret {
            Some(t) => self.resolve_ty(t, cur, f.span)?,
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
            name: fqn(cur, &f.name),
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
    /// Names bound in the innermost scope (used to detect bindings introduced by
    /// an or-pattern alternative).
    fn scope_names(&self) -> Vec<String> {
        self.scopes
            .last()
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default()
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
                    Some(t) => {
                        let cur = self.cur_mod.clone();
                        Some(self.resolve_ty(t, &cur, *span)?)
                    }
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
                // Infer the bound type from whichever side is concrete, so a
                // literal bound adopts the other's type (e.g. `0..n` with
                // `n: u32` makes `0` a `u32`). Defaults to `i32`.
                let target: Type = if !is_int_literalish(lo) {
                    let t = self.check_expr(lo, None)?.ty;
                    if !t.is_integer() {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!("for-loop bounds must be integers, found `{}`", t),
                            lo.span(),
                        ));
                    }
                    t
                } else if !is_int_literalish(hi) {
                    let t = self.check_expr(hi, None)?.ty;
                    if !t.is_integer() {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!("for-loop bounds must be integers, found `{}`", t),
                            hi.span(),
                        ));
                    }
                    t
                } else {
                    I32
                };
                let lo_ir = self.check_expr(lo, Some(&target))?;
                let hi_ir = self.check_expr(hi, Some(&target))?;
                if lo_ir.ty != hi_ir.ty {
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
            ast::Expr::TupleField { base, index, span } => {
                let base_place = self.check_place(base)?;
                let ty = self.tuple_elem_ty(base_place.ty(), *index, *span)?;
                Ok(ir::Place::TupleField {
                    base: Box::new(base_place),
                    index: *index as usize,
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

    /// The type of tuple element `index`, with a bounds check.
    fn tuple_elem_ty(&self, base_ty: &Type, index: u64, span: Span) -> Result<Type, Diagnostic> {
        match base_ty {
            Type::Tuple(ts) => ts.get(index as usize).cloned().ok_or_else(|| {
                Diagnostic::new(
                    Stage::Type,
                    format!("tuple of {} element(s) has no field `.{}`", ts.len(), index),
                    span,
                )
            }),
            other => Err(Diagnostic::new(
                Stage::Type,
                format!("cannot access `.{}` on non-tuple type `{}`", index, other),
                span,
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
            ast::Expr::Path { segments, span } => self.check_path(segments, *span),
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
            ast::Expr::TupleField { base, index, span } => {
                let base_ir = self.check_expr(base, None)?;
                let ty = self.tuple_elem_ty(&base_ir.ty, *index, *span)?;
                Ok(ir::Expr::new(
                    ir::ExprKind::TupleField {
                        base: Box::new(base_ir),
                        index: *index as usize,
                    },
                    ty,
                ))
            }
            ast::Expr::Tuple { elems, span } => {
                let elem_expected: Vec<Option<Type>> = match expected {
                    Some(Type::Tuple(ts)) if ts.len() == elems.len() => {
                        ts.iter().map(|t| Some(t.clone())).collect()
                    }
                    _ => vec![None; elems.len()],
                };
                if elems.len() < 2 {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        "a tuple must have at least two elements".to_string(),
                        *span,
                    ));
                }
                let mut irs = Vec::new();
                let mut tys = Vec::new();
                for (e, ex) in elems.iter().zip(elem_expected.iter()) {
                    let ir = self.check_expr(e, ex.as_ref())?;
                    tys.push(ir.ty.clone());
                    irs.push(ir);
                }
                Ok(ir::Expr::new(ir::ExprKind::MakeTuple(irs), Type::Tuple(tys)))
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
            ast::Expr::StructLit { path, fields, span } => {
                self.check_struct_lit(path, fields, *span)
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
        self.check_value_path(&[name.to_string()], span)
    }

    /// A path used as a value: a compile-time constant or a unit-like enum
    /// variant.
    fn check_path(&mut self, segments: &[String], span: Span) -> Result<ir::Expr, Diagnostic> {
        self.check_value_path(segments, span)
    }

    fn check_value_path(&mut self, path: &[String], span: Span) -> Result<ir::Expr, Diagnostic> {
        let cur = self.cur_mod.clone();
        // A compile-time constant.
        if let Some((cfqn, ItemKind::Const)) = self.resolve_item(&cur, path) {
            let ty = self.consts[&cfqn].ty.clone();
            return Ok(ir::Expr::new(ir::ExprKind::ConstRef(cfqn), ty));
        }
        self.check_unit_variant(path, span)
    }

    fn check_unit_variant(&mut self, path: &[String], span: Span) -> Result<ir::Expr, Diagnostic> {
        let cur = self.cur_mod.clone();
        if let Some((enum_name, tag)) = self.resolve_variant(&cur, path) {
            let variant = &self.enums[&enum_name].variants[tag];
            if !variant.fields.is_empty() {
                let shown = path.join("::");
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!(
                        "variant `{}` takes {} argument(s); call it like `{}(...)`",
                        shown,
                        variant.fields.len(),
                        shown
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
            format!("unknown identifier `{}`", path.join("::")),
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

        // Shifts: lhs determines result type; the shift amount is an
        // independent integer. A literal amount adopts the left operand's type
        // (so `x >> 1` with `x: bit<8>` keeps the amount in `bit<8>` rather than
        // defaulting to `i32`), which also keeps such code lowerable to hardware.
        if matches!(irop, ir::BinOp::Shl | ir::BinOp::Shr) {
            let l = self.check_expr(lhs, expected)?;
            if !l.ty.is_integer() {
                return Err(self.op_type_err(op, &l.ty, lhs.span()));
            }
            let r = self.check_expr(rhs, Some(&l.ty))?;
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
        let path: Vec<String> = match callee {
            ast::Expr::Ident { name, .. } => vec![name.clone()],
            ast::Expr::Path { segments, .. } => segments.clone(),
            other => {
                return Err(Diagnostic::new(
                    Stage::Type,
                    "only named functions and enum variants can be called",
                    other.span(),
                ))
            }
        };
        let shown = path.join("::");
        let cur = self.cur_mod.clone();

        // Built-in `print`.
        if path == ["print"] {
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
        if let Some((enum_name, tag)) = self.resolve_variant(&cur, &path) {
            let field_tys = self.enums[&enum_name].variants[tag].fields.clone();
            if args.len() != field_tys.len() {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!(
                        "variant `{}` takes {} argument(s), found {}",
                        shown,
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
        let (ffqn, sig) = match self.resolve_item(&cur, &path) {
            Some((f, ItemKind::Func)) => (f.clone(), self.funcs[&f].clone()),
            Some((f, _)) => {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!("`{}` is not a function", f),
                    span,
                ))
            }
            None => {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!("unknown function `{}`", shown),
                    span,
                ))
            }
        };
        if args.len() != sig.params.len() {
            return Err(Diagnostic::new(
                Stage::Type,
                format!(
                    "function `{}` takes {} argument(s), found {}",
                    shown,
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
                func: ffqn,
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
        path: &[String],
        fields: &[(String, ast::Expr)],
        span: Span,
    ) -> Result<ir::Expr, Diagnostic> {
        let cur = self.cur_mod.clone();
        let name = match self.resolve_item(&cur, path) {
            Some((f, ItemKind::Struct)) => f,
            _ => {
                return Err(Diagnostic::new(
                    Stage::Type,
                    format!("unknown struct `{}`", path.join("::")),
                    span,
                ))
            }
        };
        let name = name.as_str();
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
                    // A guard (if any) is a bool checked in the pattern's bindings.
                    let guard = match &arm.guard {
                        Some(g) => {
                            let gi = self.check_expr(g, Some(&Type::Bool))?;
                            self.expect_ty(&gi.ty, &Type::Bool, g.span(), "match guard")?;
                            Some(gi)
                        }
                        None => None,
                    };
                    let body = self.check_expr(&arm.body, result_ty.as_ref())?;
                    Ok((pat, guard, body))
                });
            self.pop_scope();
            let (pat, guard, body) = pat_result?;

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
            ir_arms.push(ir::Arm {
                pattern: pat,
                guard,
                body,
            });
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
            ast::Pattern::Range {
                lo,
                hi,
                inclusive,
                span,
            } => {
                if !ty.is_integer() {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("range pattern cannot match a value of type `{}`", ty),
                        *span,
                    ));
                }
                if !fits(*lo, ty) || !fits(*hi, ty) {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("range bound does not fit in `{}`", ty),
                        *span,
                    ));
                }
                if lo > hi || (!inclusive && lo == hi) {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        "range pattern is empty (lo must be <= hi)".to_string(),
                        *span,
                    ));
                }
                Ok(ir::Pattern::Range {
                    lo: *lo,
                    hi: *hi,
                    inclusive: *inclusive,
                })
            }
            ast::Pattern::Tuple { elems, span } => {
                let tys = match ty {
                    Type::Tuple(ts) => ts.clone(),
                    other => {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!("tuple pattern cannot match a value of type `{}`", other),
                            *span,
                        ))
                    }
                };
                if elems.len() != tys.len() {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!(
                            "tuple pattern has {} element(s) but the value has {}",
                            elems.len(),
                            tys.len()
                        ),
                        *span,
                    ));
                }
                let mut subs = Vec::new();
                for (e, t) in elems.iter().zip(tys.iter()) {
                    subs.push(self.check_pattern(e, t)?);
                }
                Ok(ir::Pattern::Tuple(subs))
            }
            ast::Pattern::Or { alts, span } => {
                // Each alternative is checked against the same type; for v1, the
                // alternatives must introduce no new bindings (keeps binding
                // types unambiguous) — except a single binding/wildcard, which is
                // not an or-pattern anyway.
                let before: Vec<String> = self.scope_names();
                let mut ir_alts = Vec::new();
                for alt in alts {
                    let a = self.check_pattern(alt, ty)?;
                    let after = self.scope_names();
                    if after.len() != before.len() {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            "or-pattern alternatives may not introduce bindings".to_string(),
                            *span,
                        ));
                    }
                    ir_alts.push(a);
                }
                Ok(ir::Pattern::Or(ir_alts))
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
                path,
                subpatterns,
                span,
            } => {
                let shown = path.join("::");
                let enum_name = match ty {
                    Type::Enum(n) => n.clone(),
                    other => {
                        return Err(Diagnostic::new(
                            Stage::Type,
                            format!("variant pattern `{}` cannot match type `{}`", shown, other),
                            *span,
                        ))
                    }
                };
                let cur = self.cur_mod.clone();
                let (ven, tag) = self.resolve_variant(&cur, path).ok_or_else(|| {
                    Diagnostic::new(Stage::Type, format!("unknown variant `{}`", shown), *span)
                })?;
                if ven != enum_name {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!("variant `{}` belongs to enum `{}`, not `{}`", shown, ven, enum_name),
                        *span,
                    ));
                }
                let field_tys = self.enums[&enum_name].variants[tag].fields.clone();
                if subpatterns.len() != field_tys.len() {
                    return Err(Diagnostic::new(
                        Stage::Type,
                        format!(
                            "variant `{}` has {} field(s) but pattern binds {}",
                            shown,
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
        // A guarded arm can fail at runtime, so it never makes a match
        // exhaustive on its own.
        let has_catch_all = arms
            .iter()
            .any(|a| a.guard.is_none() && is_irrefutable(&a.pattern));
        if has_catch_all {
            return Ok(());
        }

        match ty {
            Type::Enum(name) => {
                let ed = &self.enums[name];
                let mut covered = vec![false; ed.variants.len()];
                let cur = self.cur_mod.clone();
                // Collect variant-covering patterns, flattening or-patterns and
                // skipping guarded arms (a guard may fail).
                for a in arms {
                    if a.guard.is_some() {
                        continue;
                    }
                    for pat in flatten_or(&a.pattern) {
                        if let ast::Pattern::Variant {
                            path, subpatterns, ..
                        } = pat
                        {
                            if subpatterns.iter().all(is_irrefutable) {
                                if let Some((efqn, tag)) = self.resolve_variant(&cur, path) {
                                    if efqn == *name {
                                        covered[tag] = true;
                                    }
                                }
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
                    if a.guard.is_some() {
                        continue;
                    }
                    for pat in flatten_or(&a.pattern) {
                        if let ast::Pattern::Bool { value, .. } = pat {
                            if *value {
                                t = true;
                            } else {
                                f = true;
                            }
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

/// Join a module path and a simple name into a fully-qualified name. At the
/// program root (`cur` empty) this is just the bare name.
fn fqn(cur: &[String], name: &str) -> String {
    if cur.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", cur.join("::"), name)
    }
}

/// A pattern is irrefutable if it always matches: wildcard or binding, a tuple
/// of irrefutable patterns, or an or-pattern with an irrefutable alternative.
fn is_irrefutable(p: &ast::Pattern) -> bool {
    match p {
        ast::Pattern::Wildcard { .. } | ast::Pattern::Binding { .. } => true,
        ast::Pattern::Tuple { elems, .. } => elems.iter().all(is_irrefutable),
        ast::Pattern::Or { alts, .. } => alts.iter().any(is_irrefutable),
        _ => false,
    }
}

/// Flatten an or-pattern into its leaf alternatives (recursively); any other
/// pattern yields itself. Used for exhaustiveness over enums and bools.
fn flatten_or(p: &ast::Pattern) -> Vec<&ast::Pattern> {
    match p {
        ast::Pattern::Or { alts, .. } => alts.iter().flat_map(flatten_or).collect(),
        other => vec![other],
    }
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
    fn modules_qualify_names() {
        let src = r#"
            mod math {
                fn square(x: u32) -> u32 { x * x }
            }
            mod geom {
                struct Point { x: u32, y: u32 }
                enum Shape { Circle(u32), Rect(u32, u32) }
                fn area(s: Shape) -> u32 {
                    match s { Circle(r) => 3 * r * r, Rect(w, h) => w * h }
                }
            }
            fn main() {
                print(math::square(7));
                let p: geom::Point = geom::Point { x: 1, y: 2 };
                print(p.x + p.y);
                print(geom::area(geom::Shape::Rect(3, 4)));
            }
        "#;
        let m = ck(src).unwrap();
        // Items are keyed by fully-qualified name.
        assert!(m.funcs.contains_key("math::square"));
        assert!(m.funcs.contains_key("geom::area"));
        assert!(m.structs.contains_key("geom::Point"));
        assert!(m.enums.contains_key("geom::Shape"));
        assert!(m.funcs.contains_key("main"));
    }

    #[test]
    fn use_import_brings_name_into_scope() {
        let src = r#"
            mod math { fn dbl(x: u32) -> u32 { x + x } }
            use math::dbl;
            fn main() { print(dbl(21)); }
        "#;
        assert!(ck(src).is_ok());
    }

    #[test]
    fn glob_import_and_in_module_variant() {
        let src = r#"
            mod shapes {
                enum Shape { A, B }
                fn name(s: Shape) -> u8 { match s { A => 0, B => 1 } }
            }
            use shapes::*;
            fn pick() -> Shape { Shape::A }
            fn main() { print(shapes::name(pick())); }
        "#;
        assert!(ck(src).is_ok(), "{:?}", ck(src).err());
    }

    #[test]
    fn unknown_qualified_name_errors() {
        let msg = err_msg("mod m { fn f() -> u8 { 0 } } fn main() { print(m::g()); }");
        assert!(msg.contains("unknown function") && msg.contains("m::g"), "{}", msg);
    }

    #[test]
    fn sibling_modules_can_have_same_variant_name() {
        // Variant names need only be unique per enum now, not globally.
        let src = r#"
            mod a { enum E { Dot } fn f() -> E { E::Dot } }
            mod b { enum E { Dot } fn g() -> E { E::Dot } }
            fn main() { }
        "#;
        assert!(ck(src).is_ok(), "{:?}", ck(src).err());
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

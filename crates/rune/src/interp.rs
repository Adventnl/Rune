//! # Interpreter
//!
//! A tree-walking evaluator over the typed core [`ir`]. Because the IR is fully
//! typed and checked, the interpreter performs **no** type checks: it only
//! executes. Its job is to be *exactly* deterministic — wrapping arithmetic on
//! every integer-like type, defined runtime traps (never UB) for divide-by-zero
//! and out-of-bounds indexing — so its results match the IR's documented
//! semantics bit-for-bit.
//!
//! `print` output is captured into a buffer so programs are testable; callers
//! can also stream it to stdout.

use crate::diagnostic::{Diagnostic, Stage};
use crate::ir::{self, IntTy, Type};
use crate::span::Span;
use std::collections::HashMap;

/// A runtime value. Integer-like values are stored normalised to their type's
/// range so equality and printing are canonical.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    /// A fixed-width machine integer, stored as its true (possibly negative)
    /// value already reduced into the type's range.
    Int(i128, IntTy),
    /// A `bit<N>` value, stored masked to `N` bits.
    Bit(u128, u32),
    /// A fixed-size array.
    Array(Vec<Value>),
    /// A struct value: the struct name and its fields in definition order.
    Struct(String, Vec<Value>),
    /// An enum value: the enum name, the variant tag, and the payload.
    Enum(String, usize, Vec<Value>),
    /// A tuple value.
    Tuple(Vec<Value>),
    /// The unit value.
    Unit,
}

/// Non-local control flow inside evaluation: either an early `return` or a
/// runtime trap.
enum Signal {
    Return(Value),
    Error(Diagnostic),
}

impl From<Diagnostic> for Signal {
    fn from(d: Diagnostic) -> Self {
        Signal::Error(d)
    }
}

type EvalResult<T> = Result<T, Signal>;

/// A lexical environment: a stack of scopes mapping names to values.
type Env = Vec<HashMap<String, Value>>;

/// The interpreter owns the typed module (the swappable set of definitions) and
/// the captured `print` output.
pub struct Interpreter {
    module: ir::Module,
    output: Vec<String>,
    /// Memoised compile-time constant values (evaluated on first use).
    const_cache: HashMap<String, Value>,
}

impl Interpreter {
    pub fn new(module: ir::Module) -> Self {
        Interpreter {
            module,
            output: Vec::new(),
            const_cache: HashMap::new(),
        }
    }

    /// The module currently being interpreted.
    pub fn module(&self) -> &ir::Module {
        &self.module
    }

    /// Replace the live module (used by hot reload after swapping definitions).
    pub fn set_module(&mut self, module: ir::Module) {
        self.module = module;
        self.const_cache.clear();
    }

    /// All captured `print` lines so far.
    pub fn output(&self) -> &[String] {
        &self.output
    }

    /// Take and clear the captured output.
    pub fn take_output(&mut self) -> Vec<String> {
        std::mem::take(&mut self.output)
    }

    /// Evaluate a compile-time constant by fully-qualified name (memoised).
    pub fn eval_const(&mut self, name: &str) -> Result<Value, Diagnostic> {
        let Interpreter {
            module,
            output,
            const_cache,
        } = self;
        let mut ev = Eval {
            module,
            output,
            const_cache,
        };
        let mut env: Env = vec![HashMap::new()];
        let expr = ir::Expr::new(ir::ExprKind::ConstRef(name.to_string()), ir::Type::Unit);
        match ev.eval_expr(&expr, &mut env) {
            Ok(v) => Ok(v),
            Err(Signal::Return(v)) => Ok(v),
            Err(Signal::Error(d)) => Err(d),
        }
    }

    /// Run `main()` and return the lines it printed. Errors are returned as a
    /// single runtime [`Diagnostic`].
    pub fn run_main(&mut self) -> Result<Vec<String>, Diagnostic> {
        if !self.module.funcs.contains_key("main") {
            return Err(Diagnostic::new(
                Stage::Runtime,
                "no `main` function to run",
                Span::dummy(),
            ));
        }
        self.call("main", vec![])?;
        Ok(self.output.clone())
    }

    /// Call a function by name with already-evaluated argument values.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, Diagnostic> {
        let Interpreter {
            module,
            output,
            const_cache,
        } = self;
        let mut ev = Eval {
            module,
            output,
            const_cache,
        };
        match ev.call_func(name, args) {
            Ok(v) => Ok(v),
            Err(Signal::Return(v)) => Ok(v),
            Err(Signal::Error(d)) => Err(d),
        }
    }
}

/// The transient evaluator, holding disjoint borrows of the module and output.
struct Eval<'a> {
    module: &'a ir::Module,
    output: &'a mut Vec<String>,
    const_cache: &'a mut HashMap<String, Value>,
}

impl<'a> Eval<'a> {
    fn call_func(&mut self, name: &str, args: Vec<Value>) -> EvalResult<Value> {
        let module = self.module;
        let func = module.funcs.get(name).ok_or_else(|| {
            Diagnostic::new(
                Stage::Runtime,
                format!("call to unknown function `{}`", name),
                Span::dummy(),
            )
        })?;
        let mut scope = HashMap::new();
        for (p, v) in func.params.iter().zip(args.into_iter()) {
            scope.insert(p.name.clone(), v);
        }
        let mut env: Env = vec![scope];
        match self.eval_block(&func.body, &mut env) {
            Ok(v) => Ok(v),
            Err(Signal::Return(v)) => Ok(v),
            Err(e) => Err(e),
        }
    }

    // ---- blocks & statements -------------------------------------------

    fn eval_block(&mut self, b: &ir::Block, env: &mut Env) -> EvalResult<Value> {
        env.push(HashMap::new());
        let r = self.eval_block_inner(b, env);
        env.pop();
        r
    }

    fn eval_block_inner(&mut self, b: &ir::Block, env: &mut Env) -> EvalResult<Value> {
        for s in &b.stmts {
            self.eval_stmt(s, env)?;
        }
        match &b.tail {
            Some(e) => self.eval_expr(e, env),
            None => Ok(Value::Unit),
        }
    }

    fn eval_stmt(&mut self, s: &ir::Stmt, env: &mut Env) -> EvalResult<()> {
        match s {
            ir::Stmt::Let { name, init, .. } => {
                let v = self.eval_expr(init, env)?;
                env.last_mut().unwrap().insert(name.clone(), v);
                Ok(())
            }
            ir::Stmt::Assign { place, value } => {
                let v = self.eval_expr(value, env)?;
                self.store(place, v, env)
            }
            ir::Stmt::While { cond, body } => {
                loop {
                    match self.eval_expr(cond, env)? {
                        Value::Bool(true) => {
                            self.eval_block(body, env)?;
                        }
                        _ => break,
                    }
                }
                Ok(())
            }
            ir::Stmt::For {
                var,
                ty,
                lo,
                hi,
                body,
            } => {
                let lo_v = int_as_i128(&self.eval_expr(lo, env)?);
                let hi_v = int_as_i128(&self.eval_expr(hi, env)?);
                let mut i = lo_v;
                while i < hi_v {
                    env.push(HashMap::new());
                    env.last_mut()
                        .unwrap()
                        .insert(var.clone(), make_int_value(i, ty));
                    let r = self.eval_block(body, env);
                    env.pop();
                    r?;
                    i += 1;
                }
                Ok(())
            }
            ir::Stmt::Return { value } => {
                let v = match value {
                    Some(e) => self.eval_expr(e, env)?,
                    None => Value::Unit,
                };
                Err(Signal::Return(v))
            }
            ir::Stmt::Expr(e) => {
                self.eval_expr(e, env)?;
                Ok(())
            }
        }
    }

    // ---- assignment (functional update along the place path) -----------

    fn store(&mut self, place: &ir::Place, v: Value, env: &mut Env) -> EvalResult<()> {
        match place {
            ir::Place::Local { name, .. } => {
                for scope in env.iter_mut().rev() {
                    if scope.contains_key(name) {
                        scope.insert(name.clone(), v);
                        return Ok(());
                    }
                }
                Err(Signal::Error(Diagnostic::new(
                    Stage::Runtime,
                    format!("assignment to unbound variable `{}`", name),
                    Span::dummy(),
                )))
            }
            ir::Place::Field { base, index, .. } => {
                let mut cur = self.load(base, env)?;
                if let Value::Struct(_, fields) = &mut cur {
                    fields[*index] = v;
                } else {
                    return Err(rt("field assignment on non-struct value"));
                }
                self.store(base, cur, env)
            }
            ir::Place::TupleField { base, index, .. } => {
                let mut cur = self.load(base, env)?;
                if let Value::Tuple(elems) = &mut cur {
                    elems[*index] = v;
                } else {
                    return Err(rt("tuple assignment on non-tuple value"));
                }
                self.store(base, cur, env)
            }
            ir::Place::Index { base, index, .. } => {
                let idx = int_as_i128(&self.eval_expr(index, env)?);
                let mut cur = self.load(base, env)?;
                if let Value::Array(elems) = &mut cur {
                    let i = bounds_check(idx, elems.len())?;
                    elems[i] = v;
                } else {
                    return Err(rt("index assignment on non-array value"));
                }
                self.store(base, cur, env)
            }
        }
    }

    /// Read the current value at a place (a clone — value semantics).
    fn load(&mut self, place: &ir::Place, env: &mut Env) -> EvalResult<Value> {
        match place {
            ir::Place::Local { name, .. } => env
                .iter()
                .rev()
                .find_map(|s| s.get(name))
                .cloned()
                .ok_or_else(|| rt(&format!("read of unbound variable `{}`", name))),
            ir::Place::Field { base, index, .. } => {
                let base_v = self.load(base, env)?;
                match base_v {
                    Value::Struct(_, fields) => Ok(fields[*index].clone()),
                    _ => Err(rt("field access on non-struct value")),
                }
            }
            ir::Place::TupleField { base, index, .. } => {
                let base_v = self.load(base, env)?;
                match base_v {
                    Value::Tuple(elems) => Ok(elems[*index].clone()),
                    _ => Err(rt("tuple access on non-tuple value")),
                }
            }
            ir::Place::Index { base, index, .. } => {
                let idx = int_as_i128(&self.eval_expr(index, env)?);
                let base_v = self.load(base, env)?;
                match base_v {
                    Value::Array(elems) => {
                        let i = bounds_check(idx, elems.len())?;
                        Ok(elems[i].clone())
                    }
                    _ => Err(rt("indexing a non-array value")),
                }
            }
        }
    }

    // ---- expressions ----------------------------------------------------

    fn eval_expr(&mut self, e: &ir::Expr, env: &mut Env) -> EvalResult<Value> {
        match &e.kind {
            ir::ExprKind::Int(v) => Ok(make_int_value(*v, &e.ty)),
            ir::ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ir::ExprKind::Unit => Ok(Value::Unit),
            ir::ExprKind::Local(name) => env
                .iter()
                .rev()
                .find_map(|s| s.get(name))
                .cloned()
                .ok_or_else(|| rt(&format!("read of unbound variable `{}`", name))),
            ir::ExprKind::ConstRef(name) => {
                if let Some(v) = self.const_cache.get(name) {
                    return Ok(v.clone());
                }
                let def = self
                    .module
                    .consts
                    .get(name)
                    .ok_or_else(|| rt(&format!("reference to unknown const `{}`", name)))?;
                // Evaluate the closed initializer in a fresh environment.
                let mut cenv: Env = vec![HashMap::new()];
                let v = self.eval_expr(&def.init, &mut cenv)?;
                self.const_cache.insert(name.clone(), v.clone());
                Ok(v)
            }
            ir::ExprKind::Unary { op, expr } => {
                let v = self.eval_expr(expr, env)?;
                eval_unary(*op, v, &e.ty)
            }
            ir::ExprKind::Binary { op, lhs, rhs } => {
                // Short-circuiting logical operators.
                if matches!(op, ir::BinOp::And | ir::BinOp::Or) {
                    let l = self.eval_expr(lhs, env)?;
                    let lb = as_bool(&l)?;
                    return match op {
                        ir::BinOp::And if !lb => Ok(Value::Bool(false)),
                        ir::BinOp::Or if lb => Ok(Value::Bool(true)),
                        _ => {
                            let r = self.eval_expr(rhs, env)?;
                            Ok(Value::Bool(as_bool(&r)?))
                        }
                    };
                }
                let l = self.eval_expr(lhs, env)?;
                let r = self.eval_expr(rhs, env)?;
                eval_binary(*op, l, r, &e.ty)
            }
            ir::ExprKind::Call { func, args } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval_expr(a, env)?);
                }
                self.call_func(func, vals)
            }
            ir::ExprKind::Print { arg } => {
                let v = self.eval_expr(arg, env)?;
                self.output.push(self.format_value(&v));
                Ok(Value::Unit)
            }
            ir::ExprKind::MakeStruct { name, fields } => {
                let mut vals = Vec::with_capacity(fields.len());
                for f in fields {
                    vals.push(self.eval_expr(f, env)?);
                }
                Ok(Value::Struct(name.clone(), vals))
            }
            ir::ExprKind::MakeEnum { name, tag, args } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval_expr(a, env)?);
                }
                Ok(Value::Enum(name.clone(), *tag, vals))
            }
            ir::ExprKind::MakeTuple(elems) => {
                let mut vals = Vec::with_capacity(elems.len());
                for el in elems {
                    vals.push(self.eval_expr(el, env)?);
                }
                Ok(Value::Tuple(vals))
            }
            ir::ExprKind::Field { base, index } => {
                let v = self.eval_expr(base, env)?;
                match v {
                    Value::Struct(_, fields) => Ok(fields[*index].clone()),
                    _ => Err(rt("field access on non-struct value")),
                }
            }
            ir::ExprKind::TupleField { base, index } => {
                let v = self.eval_expr(base, env)?;
                match v {
                    Value::Tuple(elems) => Ok(elems[*index].clone()),
                    _ => Err(rt("tuple access on non-tuple value")),
                }
            }
            ir::ExprKind::Index { base, index } => {
                let base_v = self.eval_expr(base, env)?;
                let idx = int_as_i128(&self.eval_expr(index, env)?);
                match base_v {
                    Value::Array(elems) => {
                        let i = bounds_check(idx, elems.len())?;
                        Ok(elems[i].clone())
                    }
                    _ => Err(rt("indexing a non-array value")),
                }
            }
            ir::ExprKind::Array(elems) => {
                let mut vals = Vec::with_capacity(elems.len());
                for el in elems {
                    vals.push(self.eval_expr(el, env)?);
                }
                Ok(Value::Array(vals))
            }
            ir::ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let c = as_bool(&self.eval_expr(cond, env)?)?;
                if c {
                    self.eval_block(then_branch, env)
                } else {
                    self.eval_block(else_branch, env)
                }
            }
            ir::ExprKind::Match { scrutinee, arms } => {
                let v = self.eval_expr(scrutinee, env)?;
                for arm in arms {
                    let mut binds = HashMap::new();
                    if pattern_matches(&arm.pattern, &v, &mut binds) {
                        env.push(binds);
                        // A guard is evaluated in the arm's bindings; if it is
                        // false, fall through to the next arm.
                        if let Some(guard) = &arm.guard {
                            match self.eval_expr(guard, env) {
                                Ok(Value::Bool(true)) => {}
                                Ok(_) => {
                                    env.pop();
                                    continue;
                                }
                                Err(e) => {
                                    env.pop();
                                    return Err(e);
                                }
                            }
                        }
                        let r = self.eval_expr(&arm.body, env);
                        env.pop();
                        return r;
                    }
                }
                // Exhaustiveness is guaranteed by the typechecker; reaching here
                // is an internal invariant violation, reported (never UB).
                Err(rt("internal error: no match arm matched (non-exhaustive?)"))
            }
            ir::ExprKind::Block(b) => self.eval_block(b, env),
        }
    }

    // ---- printing -------------------------------------------------------

    fn format_value(&self, v: &Value) -> String {
        match v {
            Value::Bool(b) => b.to_string(),
            Value::Int(n, _) => n.to_string(),
            Value::Bit(n, _) => n.to_string(),
            Value::Unit => "()".to_string(),
            Value::Array(xs) => {
                let parts: Vec<String> = xs.iter().map(|x| self.format_value(x)).collect();
                format!("[{}]", parts.join(", "))
            }
            Value::Struct(name, fields) => {
                let names: Vec<String> = match self.module.structs.get(name) {
                    Some(sd) => sd.fields.iter().map(|f| f.name.clone()).collect(),
                    None => (0..fields.len()).map(|i| i.to_string()).collect(),
                };
                let parts: Vec<String> = names
                    .iter()
                    .zip(fields.iter())
                    .map(|(n, v)| format!("{}: {}", n, self.format_value(v)))
                    .collect();
                format!("{} {{ {} }}", name, parts.join(", "))
            }
            Value::Enum(name, tag, args) => {
                let vname = self
                    .module
                    .enums
                    .get(name)
                    .and_then(|e| e.variants.get(*tag))
                    .map(|v| v.name.clone())
                    .unwrap_or_else(|| format!("tag{}", tag));
                if args.is_empty() {
                    vname
                } else {
                    let parts: Vec<String> = args.iter().map(|a| self.format_value(a)).collect();
                    format!("{}({})", vname, parts.join(", "))
                }
            }
            Value::Tuple(elems) => {
                let parts: Vec<String> = elems.iter().map(|a| self.format_value(a)).collect();
                format!("({})", parts.join(", "))
            }
        }
    }
}

// ---- pattern matching ---------------------------------------------------

fn pattern_matches(p: &ir::Pattern, v: &Value, binds: &mut HashMap<String, Value>) -> bool {
    match p {
        ir::Pattern::Wildcard => true,
        ir::Pattern::Binding { name, .. } => {
            binds.insert(name.clone(), v.clone());
            true
        }
        ir::Pattern::Int(target) => match v {
            Value::Int(n, _) => n == target,
            Value::Bit(n, _) => (*n as i128) == *target,
            _ => false,
        },
        ir::Pattern::Bool(target) => matches!(v, Value::Bool(b) if b == target),
        ir::Pattern::Range { lo, hi, inclusive } => {
            let n = match v {
                Value::Int(n, _) => *n,
                Value::Bit(n, _) => *n as i128,
                _ => return false,
            };
            if *inclusive {
                n >= *lo && n <= *hi
            } else {
                n >= *lo && n < *hi
            }
        }
        ir::Pattern::Variant {
            tag, subpatterns, ..
        } => match v {
            Value::Enum(_, vtag, args) if vtag == tag && args.len() == subpatterns.len() => {
                subpatterns
                    .iter()
                    .zip(args.iter())
                    .all(|(sp, a)| pattern_matches(sp, a, binds))
            }
            _ => false,
        },
        ir::Pattern::Tuple(subs) => match v {
            Value::Tuple(elems) if elems.len() == subs.len() => subs
                .iter()
                .zip(elems.iter())
                .all(|(sp, a)| pattern_matches(sp, a, binds)),
            _ => false,
        },
        ir::Pattern::Or(alts) => alts.iter().any(|alt| pattern_matches(alt, v, binds)),
    }
}

// ---- arithmetic helpers -------------------------------------------------

fn rt(msg: &str) -> Signal {
    Signal::Error(Diagnostic::new(Stage::Runtime, msg, Span::dummy()))
}

fn as_bool(v: &Value) -> EvalResult<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        _ => Err(rt("expected a boolean value")),
    }
}

/// Read any integer-like value as an `i128` (for indexing and loop counters).
fn int_as_i128(v: &Value) -> i128 {
    match v {
        Value::Int(n, _) => *n,
        Value::Bit(n, _) => *n as i128,
        Value::Bool(b) => *b as i128,
        _ => 0,
    }
}

fn bounds_check(idx: i128, len: usize) -> EvalResult<usize> {
    if idx < 0 || idx as u128 >= len as u128 {
        return Err(rt(&format!(
            "array index {} out of bounds for length {}",
            idx, len
        )));
    }
    Ok(idx as usize)
}

fn int_mask(w: u32) -> u128 {
    if w >= 128 {
        u128::MAX
    } else {
        (1u128 << w) - 1
    }
}

/// Reinterpret the low `ty.width` bits of `m` as a value of integer type `ty`.
fn from_bits(m: u128, ty: IntTy) -> i128 {
    let w = ty.width;
    let masked = m & int_mask(w);
    if ty.signed && w < 128 && (masked & (1u128 << (w - 1))) != 0 {
        (masked as i128) - (1i128 << w)
    } else {
        masked as i128
    }
}

/// Normalise a true integer value into type `ty`'s range (wrapping).
fn norm_int(v: i128, ty: IntTy) -> i128 {
    from_bits(v as u128, ty)
}

/// Build a [`Value`] from a raw integer for the given integer-like type.
fn make_int_value(v: i128, ty: &Type) -> Value {
    match ty {
        Type::Int(it) => Value::Int(norm_int(v, *it), *it),
        Type::Bit(n) => Value::Bit((v as u128) & int_mask(*n), *n),
        // Should not happen for well-typed IR; fall back to a defined value.
        _ => Value::Int(v, IntTy { signed: true, width: 32 }),
    }
}

fn eval_unary(op: ir::UnOp, v: Value, _ty: &Type) -> EvalResult<Value> {
    match op {
        ir::UnOp::Neg => match v {
            Value::Int(n, it) => Ok(Value::Int(norm_int(n.wrapping_neg(), it), it)),
            Value::Bit(n, w) => Ok(Value::Bit(n.wrapping_neg() & int_mask(w), w)),
            _ => Err(rt("cannot negate this value")),
        },
        ir::UnOp::Not => match v {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            // Bitwise NOT, then reinterpret within the type's width.
            Value::Int(n, it) => Ok(Value::Int(from_bits(!(n as u128), it), it)),
            Value::Bit(n, w) => Ok(Value::Bit((!n) & int_mask(w), w)),
            _ => Err(rt("cannot apply `!` to this value")),
        },
    }
}

fn eval_binary(op: ir::BinOp, l: Value, r: Value, ty: &Type) -> EvalResult<Value> {
    use ir::BinOp::*;

    // Comparisons produce a bool regardless of operand type.
    if op.is_comparison() {
        let res = match (&l, &r) {
            (Value::Int(a, _), Value::Int(b, _)) => compare(op, a.cmp(b)),
            (Value::Bit(a, _), Value::Bit(b, _)) => compare(op, a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => match op {
                Eq => a == b,
                Ne => a != b,
                _ => return Err(rt("cannot order booleans")),
            },
            _ => return Err(rt("comparison of incompatible values")),
        };
        return Ok(Value::Bool(res));
    }

    // Shifts: the result type follows the left operand; the shift amount may be
    // an independently-typed integer, so handle them before same-type pairing.
    if matches!(op, Shl | Shr) {
        let amount = int_as_i128(&r);
        return match l {
            Value::Int(a, it) => {
                let raw = int_arith(op, a, amount, it.signed)?;
                Ok(Value::Int(norm_int(raw, it), it))
            }
            Value::Bit(a, w) => {
                let raw = bit_arith(op, a, amount as u128, w)?;
                Ok(Value::Bit(raw & int_mask(w), w))
            }
            _ => Err(rt("shift of a non-integer value")),
        };
    }

    match (l, r) {
        (Value::Int(a, it), Value::Int(b, _)) => {
            let raw = int_arith(op, a, b, it.signed)?;
            Ok(Value::Int(norm_int(raw, it), it))
        }
        (Value::Bit(a, w), Value::Bit(b, _)) => {
            let raw = bit_arith(op, a, b, w)?;
            Ok(Value::Bit(raw & int_mask(w), w))
        }
        _ => {
            let _ = ty;
            Err(rt("arithmetic on incompatible values"))
        }
    }
}

fn compare(op: ir::BinOp, ord: std::cmp::Ordering) -> bool {
    use ir::BinOp::*;
    use std::cmp::Ordering::*;
    match op {
        Eq => ord == Equal,
        Ne => ord != Equal,
        Lt => ord == Less,
        Le => ord != Greater,
        Gt => ord == Greater,
        Ge => ord != Less,
        _ => false,
    }
}

/// Integer arithmetic/bitwise/shift on machine integers. Returns the *raw*
/// (un-normalised) result; the caller normalises into the type.
fn int_arith(op: ir::BinOp, a: i128, b: i128, signed: bool) -> EvalResult<i128> {
    use ir::BinOp::*;
    Ok(match op {
        Add => a.wrapping_add(b),
        Sub => a.wrapping_sub(b),
        Mul => a.wrapping_mul(b),
        Div => {
            if b == 0 {
                return Err(rt("division by zero"));
            }
            a.wrapping_div(b)
        }
        Rem => {
            if b == 0 {
                return Err(rt("remainder by zero"));
            }
            a.wrapping_rem(b)
        }
        BitAnd => a & b,
        BitOr => a | b,
        BitXor => a ^ b,
        Shl => {
            let amt = (b.rem_euclid(128)) as u32;
            ((a as u128) << amt) as i128
        }
        Shr => {
            let amt = (b.rem_euclid(128)) as u32;
            if signed {
                a >> amt
            } else {
                ((a as u128) >> amt) as i128
            }
        }
        _ => return Err(rt("unsupported integer operator")),
    })
}

/// Arithmetic/bitwise/shift on `bit<N>` values, wrapping modulo `2^N`. Returns
/// the raw bits; the caller masks to `N`.
fn bit_arith(op: ir::BinOp, a: u128, b: u128, w: u32) -> EvalResult<u128> {
    use ir::BinOp::*;
    Ok(match op {
        Add => a.wrapping_add(b),
        Sub => a.wrapping_sub(b),
        Mul => a.wrapping_mul(b),
        Div => {
            if b == 0 {
                return Err(rt("division by zero"));
            }
            a / b
        }
        Rem => {
            if b == 0 {
                return Err(rt("remainder by zero"));
            }
            a % b
        }
        BitAnd => a & b,
        BitOr => a | b,
        BitXor => a ^ b,
        Shl => {
            let amt = (b % (w as u128).max(1)) as u32;
            a << amt
        }
        Shr => {
            let amt = (b % (w as u128).max(1)) as u32;
            a >> amt
        }
        _ => return Err(rt("unsupported bit operator")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile;

    fn run(src: &str) -> Vec<String> {
        let module = compile(src).expect("should typecheck");
        let mut interp = Interpreter::new(module);
        interp.run_main().expect("should run")
    }

    #[test]
    fn const_items_evaluate_at_compile_time() {
        let src = r#"
            const WIDTH: u32 = 8;
            const DOUBLE: u32 = WIDTH * 2;
            const MASK: bit<8> = 0xFF;
            mod cfg { const VERSION: u32 = 3; }
            fn masked(x: bit<8>) -> bit<8> { x & MASK }
            fn main() {
                print(WIDTH);
                print(DOUBLE);
                print(masked(200));
                print(cfg::VERSION);
            }
        "#;
        assert_eq!(run(src), vec!["8", "16", "200", "3"]);
    }

    #[test]
    fn tuples_construct_index_destructure() {
        let src = r#"
            fn divmod(a: u32, b: u32) -> (u32, u32) { (a / b, a % b) }
            fn main() {
                let q: (u32, u32) = divmod(17, 5);
                print(q.0);
                print(q.1);
                let (d, m) = divmod(20, 6);
                print(d);
                print(m);
                print(q);
            }
        "#;
        assert_eq!(run(src), vec!["3", "2", "3", "2", "(3, 2)"]);
    }

    #[test]
    fn mutable_tuple_field_assignment() {
        let src = r#"
            fn main() {
                let mut p: (u32, u32) = (1, 2);
                p.0 = p.0 + 10;
                print(p.0);
                print(p.1);
            }
        "#;
        assert_eq!(run(src), vec!["11", "2"]);
    }

    #[test]
    fn match_guards_or_and_range_patterns() {
        let src = r#"
            fn classify(n: u32) -> u32 {
                match n {
                    0 => 100,
                    1 | 2 | 3 => 200,
                    4..=9 => 300,
                    x if x > 100 => 999,
                    _ => 0,
                }
            }
            fn main() {
                print(classify(0));
                print(classify(2));
                print(classify(7));
                print(classify(9));
                print(classify(500));
                print(classify(42));
            }
        "#;
        assert_eq!(run(src), vec!["100", "200", "300", "300", "999", "0"]);
    }

    #[test]
    fn half_open_range_pattern_excludes_high() {
        let src = r#"
            fn f(n: u8) -> u8 { match n { 0..2 => 1, _ => 0 } }
            fn main() { print(f(0)); print(f(1)); print(f(2)); }
        "#;
        assert_eq!(run(src), vec!["1", "1", "0"]);
    }

    #[test]
    fn nested_tuple_pattern_in_match() {
        let src = r#"
            fn first_positive(p: (bool, u32)) -> u32 {
                match p {
                    (true, x) => x,
                    _ => 0,
                }
            }
            fn main() {
                print(first_positive((true, 42)));
                print(first_positive((false, 42)));
            }
        "#;
        assert_eq!(run(src), vec!["42", "0"]);
    }

    #[test]
    fn milestone_program_runs() {
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
        assert_eq!(run(src), vec!["44", "12", "12"]);
    }

    #[test]
    fn bit_wrapping_is_exact() {
        let src = "fn main() { let a: bit<8> = 200; let b: bit<8> = 100; print(a + b); }";
        assert_eq!(run(src), vec!["44"]);
    }

    #[test]
    fn signed_wraps_two_complement() {
        let src = "fn main() { let a: i8 = 127; print(a + 1); }";
        assert_eq!(run(src), vec!["-128"]);
    }

    #[test]
    fn for_loop_sums_array() {
        let src = r#"
            fn main() {
                let a: [u32; 4] = [1, 2, 3, 4];
                let mut s: u32 = 0;
                for i in 0..4 { s = s + a[i]; }
                print(s);
            }
        "#;
        assert_eq!(run(src), vec!["10"]);
    }

    #[test]
    fn while_loop_and_if() {
        let src = r#"
            fn main() {
                let mut n: i32 = 5;
                let mut acc: i32 = 1;
                while n > 0 { acc = acc * n; n = n - 1; }
                print(acc);
            }
        "#;
        assert_eq!(run(src), vec!["120"]);
    }

    #[test]
    fn recursion_works() {
        let src = r#"
            fn fib(n: u32) -> u32 {
                if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
            }
            fn main() { print(fib(10)); }
        "#;
        assert_eq!(run(src), vec!["55"]);
    }

    #[test]
    fn struct_field_mutation() {
        let src = r#"
            struct P { x: u32, y: u32 }
            fn main() {
                let mut p = P { x: 1, y: 2 };
                p.x = p.x + 10;
                print(p.x);
                print(p.y);
            }
        "#;
        assert_eq!(run(src), vec!["11", "2"]);
    }

    #[test]
    fn divide_by_zero_is_trapped() {
        let module = compile("fn main() { let z: i32 = 0; print(1 / z); }").unwrap();
        let mut interp = Interpreter::new(module);
        let err = interp.run_main().unwrap_err();
        assert_eq!(err.stage, Stage::Runtime);
        assert!(err.message.contains("division by zero"));
    }

    #[test]
    fn index_out_of_bounds_is_trapped() {
        let module =
            compile("fn main() { let a: [u8; 2] = [1, 2]; let i: u32 = 5; print(a[i]); }").unwrap();
        let mut interp = Interpreter::new(module);
        let err = interp.run_main().unwrap_err();
        assert!(err.message.contains("out of bounds"));
    }

    #[test]
    fn bitwise_and_shift() {
        let src = r#"
            fn main() {
                let a: bit<8> = 0xF0;
                let b: bit<8> = 0x0F;
                print(a | b);
                print(a & b);
                print(a >> 4);
                print(b << 4);
            }
        "#;
        assert_eq!(run(src), vec!["255", "0", "15", "240"]);
    }
}

//! # Verilog backend
//!
//! Lowers the **synthesizable subset** of the typed core [`crate::ir`] to
//! synthesizable Verilog. Codegen is *gated* by the analysis pass
//! ([`crate::hdl::analyze`]): a function is only lowered if that pass marks it
//! synthesizable. The interpreter ([`crate::interp`]) is the executable
//! specification — generated hardware must agree with it **bit-for-bit**.
//!
//! ## Equivalence by construction
//!
//! Rune integer-like arithmetic *wraps* at each operation (`bit<N>` is mod
//! `2^N`). Verilog, by contrast, evaluates an expression at a single
//! context-determined width and only truncates at the final assignment — so a
//! naive `(a + b) * c` would keep full precision for `a + b`. To match the
//! interpreter exactly, the lowering is in **SSA form**: every IR operation
//! becomes its own sized `wire`, which forces truncation to that operation's
//! width at each step. Shift amounts are reduced modulo the width (mask for
//! power-of-two widths) to mirror the interpreter's total shifts.
//!
//! ## Module convention
//!
//! Each lowered function becomes a Verilog `module`: one `input` port per
//! parameter (named after the parameter) and a single `output` port named
//! `out`. Calls become module instantiations. Fully-qualified names are
//! sanitized (`std::bits::rotl32` → `std__bits__rotl32`).
//!
//! ## Scope (this phase)
//!
//! Scalar hardware types (`bit<N>`, `bool`), all operators, immutable `let`,
//! `if`/`match` as value expressions, and calls. **Aggregates are now lowered**:
//! `struct`/`enum`/array construction, field/element/payload access, enum
//! `match` (variant patterns), bounded `for` loops (constant-bound unrolling),
//! and mutable locals with phi merges over `if`/`match` branches.
//!
//! ### Bit packing (exact, must match the interpreter packing in tests)
//!
//! * **struct** width = sum of field widths; field 0 occupies the LOW bits,
//!   fields ascending (Verilog `{f_{n-1}, …, f1, f0}`).
//! * **array `[T; N]`** width = `N * width(T)`; element 0 in the LOW bits.
//! * **enum** width = `tag_bits + payload_bits`, where
//!   `tag_bits = ceil(log2(num_variants))` (`0` for a single variant) and
//!   `payload_bits = max` over variants of that variant's packed field widths.
//!   Layout is `value = (tag << payload_bits) | payload`; a variant's payload is
//!   its fields packed field-0-low, zero-extended to `payload_bits`.
//!
//! `while` and unit `return` remain unsupported-for-codegen (distinct from "not
//! synthesizable"). **No codegen for anything the analysis pass rejects.**

use crate::hdl;
use crate::ir::{self, BinOp, Type, UnOp};
use std::collections::HashMap;
use std::fmt::Write as _;

// ===================================================================
// Phase A — the frozen Verilog netlist model
// ===================================================================

/// A complete generated design: a set of Verilog modules.
#[derive(Clone, Debug, PartialEq)]
pub struct Design {
    pub modules: Vec<Module>,
}

/// A single Verilog module.
#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    /// Sanitized Verilog identifier.
    pub name: String,
    pub ports: Vec<Port>,
    /// Body items, emitted in order (declarations before use, SSA style).
    pub items: Vec<Item>,
}

impl Module {
    pub fn input_ports(&self) -> impl Iterator<Item = &Port> {
        self.ports.iter().filter(|p| p.dir == Dir::Input)
    }
    pub fn output_port(&self) -> &Port {
        self.ports.iter().find(|p| p.dir == Dir::Output).unwrap()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Input,
    Output,
}

/// A module port. `width >= 1`; width 1 emits a scalar (no `[0:0]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    pub dir: Dir,
    pub name: String,
    pub width: u32,
}

/// A module body item.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// `wire [w-1:0] name = value;`
    Wire {
        name: String,
        width: u32,
        value: Expr,
    },
    /// `assign lhs = rhs;`
    Assign { lhs: String, rhs: Expr },
    /// Instantiate a submodule, binding input ports to expressions and the
    /// `out` port to a freshly-declared wire `out_wire`.
    Instance {
        module: String,
        inst: String,
        /// (port name, driving expression)
        conns: Vec<(String, Expr)>,
        out_wire: String,
        out_width: u32,
    },
}

/// A Verilog expression. Operations carry the original IR operator so a single
/// definition drives both text emission and the reference evaluator.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A sized literal, e.g. `8'd44`.
    Const { value: u128, width: u32 },
    /// A reference to a port or wire.
    Ref(String),
    Bin {
        op: BinOp,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    Un {
        op: UnOp,
        e: Box<Expr>,
    },
    /// `c ? t : e`
    Ternary {
        c: Box<Expr>,
        t: Box<Expr>,
        e: Box<Expr>,
    },
    /// Bit concatenation `{a, b, …}`. The first element is the **most**
    /// significant (Verilog convention), so callers must order parts high-to-low.
    /// Each part carries its own width so the reference evaluator can place it.
    Concat(Vec<(Expr, u32)>),
    /// A bit slice `x[hi:lo]` (inclusive, `hi >= lo`). Width-1 slices emit `x[i]`.
    Slice {
        e: Box<Expr>,
        hi: u32,
        lo: u32,
    },
}

// ---- text emission ------------------------------------------------------

impl std::fmt::Display for Design {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, m) in self.modules.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{}", m)?;
        }
        Ok(())
    }
}

fn decl_range(width: u32) -> String {
    if width <= 1 {
        String::new()
    } else {
        format!("[{}:0] ", width - 1)
    }
}

impl std::fmt::Display for Module {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ports: Vec<String> = self
            .ports
            .iter()
            .map(|p| {
                let dir = match p.dir {
                    Dir::Input => "input",
                    Dir::Output => "output",
                };
                format!("{} {}{}", dir, decl_range(p.width), p.name)
            })
            .collect();
        writeln!(f, "module {} (", self.name)?;
        writeln!(f, "    {}", ports.join(",\n    "))?;
        writeln!(f, ");")?;
        for item in &self.items {
            match item {
                Item::Wire { name, width, value } => {
                    writeln!(f, "    wire {}{} = {};", decl_range(*width), name, value)?;
                }
                Item::Assign { lhs, rhs } => {
                    writeln!(f, "    assign {} = {};", lhs, rhs)?;
                }
                Item::Instance {
                    module,
                    inst,
                    conns,
                    out_wire,
                    out_width,
                } => {
                    writeln!(f, "    wire {}{};", decl_range(*out_width), out_wire)?;
                    write!(f, "    {} {} (", module, inst)?;
                    let mut parts: Vec<String> =
                        conns.iter().map(|(p, e)| format!(".{}({})", p, e)).collect();
                    parts.push(format!(".out({})", out_wire));
                    writeln!(f, "{});", parts.join(", "))?;
                }
            }
        }
        write!(f, "endmodule")
    }
}

impl std::fmt::Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::Const { value, width } => write!(f, "{}'d{}", (*width).max(1), value),
            Expr::Ref(n) => write!(f, "{}", n),
            Expr::Bin { op, l, r } => write!(f, "({} {} {})", l, binop_text(*op), r),
            Expr::Un { op, e } => write!(f, "({}{})", unop_text(*op), e),
            Expr::Ternary { c, t, e } => write!(f, "({} ? {} : {})", c, t, e),
            Expr::Concat(parts) => {
                let rendered: Vec<String> = parts.iter().map(|(p, _)| p.to_string()).collect();
                write!(f, "{{{}}}", rendered.join(", "))
            }
            Expr::Slice { e, hi, lo } => {
                if hi == lo {
                    write!(f, "{}[{}]", e, lo)
                } else {
                    write!(f, "{}[{}:{}]", e, hi, lo)
                }
            }
        }
    }
}

fn binop_text(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
    }
}

fn unop_text(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "~",
    }
}

// ===================================================================
// Phase B — lowering IR -> Verilog netlist
// ===================================================================

/// The outcome of lowering a whole module.
#[derive(Clone, Debug)]
pub struct LowerResult {
    pub design: Design,
    /// Functions that were not lowered, with a human-readable reason
    /// (`(fully-qualified name, reason)`), e.g. not synthesizable, or uses a
    /// construct codegen does not yet support.
    pub skipped: Vec<(String, String)>,
}

/// Lower every synthesizable function in `module` to Verilog. Functions the
/// analysis pass rejects — or that use constructs this phase does not yet
/// support — are recorded in `skipped`, never mis-lowered.
pub fn lower(module: &ir::Module) -> LowerResult {
    let reports = hdl::analyze(module);
    let synth: std::collections::HashSet<&str> = reports
        .iter()
        .filter(|r| r.synthesizable)
        .map(|r| r.name.as_str())
        .collect();

    let mut modules = Vec::new();
    let mut skipped = Vec::new();

    // Deterministic order (funcs is a BTreeMap).
    for (name, func) in &module.funcs {
        if !synth.contains(name.as_str()) {
            let reasons = reports
                .iter()
                .find(|r| &r.name == name)
                .map(|r| r.reasons.join("; "))
                .unwrap_or_else(|| "not synthesizable".to_string());
            skipped.push((name.clone(), reasons));
            continue;
        }
        match lower_func(func, module) {
            Ok(m) => modules.push(m),
            Err(reason) => skipped.push((name.clone(), format!("codegen unsupported: {}", reason))),
        }
    }

    skipped.sort();
    LowerResult {
        design: Design { modules },
        skipped,
    }
}

/// Emit the Verilog text for a module's synthesizable functions (header +
/// `skipped` comments + modules).
pub fn emit(module: &ir::Module) -> String {
    let result = lower(module);
    let mut out = String::new();
    out.push_str("// Generated by the Rune Verilog backend (synthesizable subset only).\n");
    out.push_str("// Semantics match the Rune interpreter bit-for-bit.\n\n");
    for (name, reason) in &result.skipped {
        let _ = writeln!(out, "// skipped `{}`: {}", name, reason);
    }
    if !result.skipped.is_empty() {
        out.push('\n');
    }
    let _ = write!(out, "{}", result.design);
    if !result.design.modules.is_empty() {
        out.push('\n');
    }
    out
}

/// The packed bit width of a *hardware* type, or `Err` if it is not lowerable.
///
/// Aggregate packing (must match the interpreter packing used in tests):
/// * struct = sum of field widths (field 0 in the low bits),
/// * array `[T; N]` = `N * width(T)` (element 0 in the low bits),
/// * enum = `tag_bits + payload_bits` (payload low, tag high).
fn ty_width(t: &Type, module: &ir::Module) -> Result<u32, String> {
    match t {
        Type::Bool => Ok(1),
        Type::Bit(n) => Ok(*n),
        Type::Int(it) => Err(format!("machine integer `{}` is not a hardware type", it_name(it))),
        Type::Array(elem, n) => {
            let ew = ty_width(elem, module)?;
            Ok(ew.checked_mul(*n as u32).ok_or("array width overflow")?)
        }
        Type::Struct(name) => {
            let def = module
                .structs
                .get(name)
                .ok_or_else(|| format!("unknown struct `{}`", name))?;
            let mut total = 0u32;
            for f in &def.fields {
                total += ty_width(&f.ty, module)?;
            }
            Ok(total)
        }
        Type::Enum(name) => {
            let def = module
                .enums
                .get(name)
                .ok_or_else(|| format!("unknown enum `{}`", name))?;
            Ok(enum_tag_bits(def) + enum_payload_bits(def, module)?)
        }
        Type::Tuple(ts) => {
            let mut total = 0u32;
            for t in ts {
                total += ty_width(t, module)?;
            }
            Ok(total)
        }
        Type::Unit => Err("unit type cannot be a hardware value".to_string()),
    }
}

/// Bit offset (low end) of tuple element `index` within its packed value.
fn tuple_elem_offset(ts: &[Type], index: usize, module: &ir::Module) -> Result<u32, String> {
    let mut off = 0u32;
    for t in &ts[..index] {
        off += ty_width(t, module)?;
    }
    Ok(off)
}

/// Number of bits needed to hold the variant tag: `ceil(log2(num_variants))`,
/// or `0` when there is a single (or no) variant.
fn enum_tag_bits(def: &ir::EnumDef) -> u32 {
    let n = def.variants.len();
    if n <= 1 {
        0
    } else {
        // ceil(log2(n))
        usize::BITS - (n - 1).leading_zeros()
    }
}

/// The packed width of one variant's payload (its fields, field-0-low).
fn variant_payload_width(v: &ir::Variant, module: &ir::Module) -> Result<u32, String> {
    let mut w = 0u32;
    for t in &v.fields {
        w += ty_width(t, module)?;
    }
    Ok(w)
}

/// `payload_bits` = max packed payload width across all variants.
fn enum_payload_bits(def: &ir::EnumDef, module: &ir::Module) -> Result<u32, String> {
    let mut max = 0u32;
    for v in &def.variants {
        max = max.max(variant_payload_width(v, module)?);
    }
    Ok(max)
}

/// The byte offset (in bits) of struct field `index`: sum of widths of fields
/// before it.
fn struct_field_offset(
    def: &ir::StructDef,
    index: usize,
    module: &ir::Module,
) -> Result<u32, String> {
    let mut off = 0u32;
    for f in def.fields.iter().take(index) {
        off += ty_width(&f.ty, module)?;
    }
    Ok(off)
}

fn it_name(it: &ir::IntTy) -> String {
    format!("{}{}", if it.signed { "i" } else { "u" }, it.width)
}

/// Convert a fully-qualified Rune name into a valid Verilog identifier.
pub fn sanitize(name: &str) -> String {
    let mut s = String::new();
    let replaced = name.replace("::", "__");
    for ch in replaced.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            s.push(ch);
        } else {
            s.push('_');
        }
    }
    if s.is_empty() || s.chars().next().unwrap().is_ascii_digit() {
        s.insert(0, '_');
    }
    s
}

/// A lowered value: a Verilog expression plus its bit width.
#[derive(Clone)]
struct Net {
    expr: Expr,
    width: u32,
}

struct Lowerer<'a> {
    module: &'a ir::Module,
    items: Vec<Item>,
    temp: usize,
    /// local name -> current net (immutable bindings + params + mutable locals,
    /// rebound on assignment — SSA).
    env: HashMap<String, Net>,
    /// Compile-time constant bindings (loop counters), used for `for` bounds and
    /// constant array indices.
    const_env: HashMap<String, i128>,
}

fn lower_func(func: &ir::Func, module: &ir::Module) -> Result<Module, String> {
    let mut ports = Vec::new();
    let mut lo = Lowerer {
        module,
        items: Vec::new(),
        temp: 0,
        env: HashMap::new(),
        const_env: HashMap::new(),
    };

    for p in &func.params {
        if p.name == "out" {
            return Err("a parameter named `out` collides with the output port".to_string());
        }
        let width = ty_width(&p.ty, module)?;
        ports.push(Port {
            dir: Dir::Input,
            name: p.name.clone(),
            width,
        });
        lo.env.insert(
            p.name.clone(),
            Net {
                expr: Expr::Ref(p.name.clone()),
                width,
            },
        );
    }
    let ret_width = ty_width(&func.ret, module)?;
    ports.push(Port {
        dir: Dir::Output,
        name: "out".to_string(),
        width: ret_width,
    });

    let result = lo.lower_block(&func.body)?;
    if result.width != ret_width {
        return Err(format!(
            "internal width mismatch: body {} vs return {}",
            result.width, ret_width
        ));
    }
    lo.items.push(Item::Assign {
        lhs: "out".to_string(),
        rhs: result.expr,
    });

    Ok(Module {
        name: sanitize(&func.name),
        ports,
        items: lo.items,
    })
}

impl<'a> Lowerer<'a> {
    fn fresh(&mut self, prefix: &str) -> String {
        let n = format!("{}{}", prefix, self.temp);
        self.temp += 1;
        n
    }

    /// Materialise an expression into a sized wire so it is truncated to
    /// `width` — the key to bit-exact wrapping.
    fn emit(&mut self, width: u32, expr: Expr) -> Net {
        let name = self.fresh("w");
        self.items.push(Item::Wire {
            name: name.clone(),
            width,
            value: expr,
        });
        Net {
            expr: Expr::Ref(name),
            width,
        }
    }

    /// Lower a block that yields a value (the body of a function, an `if`/`match`
    /// arm, a block expression). New `let` bindings are block-scoped: they are
    /// removed (and shadows restored) on exit, but **assignments to outer
    /// mutable locals persist** so accumulator-style code threads correctly.
    fn lower_block(&mut self, b: &ir::Block) -> Result<Net, String> {
        let saved = self.env.clone();
        let mut introduced: Vec<String> = Vec::new();
        let early = self.lower_stmts(&b.stmts, &mut introduced)?;
        let net = match early {
            Some(n) => n,
            None => match &b.tail {
                Some(e) => self.lower_expr(e)?,
                None => return Err("block has no value to lower".to_string()),
            },
        };
        // Restore block-scoped names (those introduced by this block's `let`s),
        // leaving assignments to pre-existing locals in place.
        for name in introduced {
            match saved.get(&name) {
                Some(prev) => {
                    self.env.insert(name, prev.clone());
                }
                None => {
                    self.env.remove(&name);
                }
            }
        }
        Ok(net)
    }

    /// Lower a sequence of statements, threading the **mutable** env. Records the
    /// names freshly introduced by `let` into `introduced` (so the caller can
    /// scope them). Returns `Some(net)` if an early `return` produced a value.
    fn lower_stmts(
        &mut self,
        stmts: &[ir::Stmt],
        introduced: &mut Vec<String>,
    ) -> Result<Option<Net>, String> {
        for s in stmts {
            match s {
                ir::Stmt::Let { name, init, .. } => {
                    let net = self.lower_expr(init)?;
                    let wname = self.fresh(&format!("v{}_", sanitize(name)));
                    self.items.push(Item::Wire {
                        name: wname.clone(),
                        width: net.width,
                        value: net.expr,
                    });
                    if !introduced.contains(name) {
                        introduced.push(name.clone());
                    }
                    self.env.insert(
                        name.clone(),
                        Net {
                            expr: Expr::Ref(wname),
                            width: net.width,
                        },
                    );
                }
                ir::Stmt::Expr(e) => {
                    // An `if`/`match` in statement position can reassign outer
                    // locals; this needs a phi merge. Pure value expressions have
                    // no observable hardware effect.
                    self.lower_unit_expr(e)?;
                }
                ir::Stmt::Return { value } => {
                    return Ok(Some(match value {
                        Some(e) => self.lower_expr(e)?,
                        None => return Err("`return` of unit is not lowerable".to_string()),
                    }));
                }
                ir::Stmt::Assign { place, value } => {
                    self.lower_assign(place, value)?;
                }
                ir::Stmt::While { .. } => {
                    return Err("`while` loops are not synthesizable".to_string())
                }
                ir::Stmt::For {
                    var,
                    ty,
                    lo,
                    hi,
                    body,
                } => {
                    self.lower_for(var, ty, lo, hi, body)?;
                }
            }
        }
        Ok(None)
    }

    /// Lower an expression evaluated **for effect only** (unit context). `if` /
    /// `match` / `block` may reassign outer locals and so go through the phi
    /// paths; a non-unit value expression has no observable hardware effect.
    fn lower_unit_expr(&mut self, e: &ir::Expr) -> Result<(), String> {
        match &e.kind {
            ir::ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => self.lower_if_stmt(cond, then_branch, else_branch),
            ir::ExprKind::Match { scrutinee, arms } if e.ty == Type::Unit => {
                self.lower_match_stmt(scrutinee, arms)
            }
            ir::ExprKind::Block(b) => {
                let saved = self.env.clone();
                let mut inner = Vec::new();
                self.lower_effect_block(b, &mut inner)?;
                for name in inner {
                    match saved.get(&name) {
                        Some(prev) => {
                            self.env.insert(name, prev.clone());
                        }
                        None => {
                            self.env.remove(&name);
                        }
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Lower a **unit-typed** block for effect: its statements, then a unit tail
    /// (an `if`/`match` in tail position behaves like a statement). New `let`
    /// names are appended to `introduced` for the caller to scope.
    fn lower_effect_block(
        &mut self,
        b: &ir::Block,
        introduced: &mut Vec<String>,
    ) -> Result<(), String> {
        self.lower_stmts(&b.stmts, introduced)?;
        if let Some(t) = &b.tail {
            if t.ty == Type::Unit {
                self.lower_unit_expr(t)?;
            } else {
                // A non-unit tail in effect position: still lower it (its wires
                // may be referenced indirectly), but it produces no binding.
                self.lower_expr(t)?;
            }
        }
        Ok(())
    }

    /// Unroll a bounded `for` loop. The bounds must be constant; each iteration
    /// binds `var` to a constant net and lowers the body, threading the mutable
    /// env across iterations.
    fn lower_for(
        &mut self,
        var: &str,
        ty: &Type,
        lo: &ir::Expr,
        hi: &ir::Expr,
        body: &ir::Block,
    ) -> Result<(), String> {
        let lo_v = self
            .const_eval(lo)
            .ok_or("non-constant for-loop bound")?;
        let hi_v = self
            .const_eval(hi)
            .ok_or("non-constant for-loop bound")?;
        // The loop variable's net width: use its (hardware) type width if it is
        // one, else a 64-bit constant (it can only feed const-eval / arithmetic
        // that the interpreter also wraps).
        let var_width = ty_width(ty, self.module).unwrap_or(64);

        let saved_var = self.env.get(var).cloned();
        let saved_const = self.const_env.get(var).copied();

        let mut i = lo_v;
        while i < hi_v {
            self.const_env.insert(var.to_string(), i);
            self.env.insert(
                var.to_string(),
                Net {
                    expr: Expr::Const {
                        value: (i as u128) & mask(var_width),
                        width: var_width,
                    },
                    width: var_width,
                },
            );
            // The body is a unit block: scope its lets, persist its assigns.
            let saved = self.env.clone();
            let mut inner = Vec::new();
            self.lower_effect_block(body, &mut inner)?;
            for name in inner {
                match saved.get(&name) {
                    Some(prev) => {
                        self.env.insert(name, prev.clone());
                    }
                    None => {
                        self.env.remove(&name);
                    }
                }
            }
            i += 1;
        }

        // Restore the loop variable's binding.
        match saved_var {
            Some(v) => {
                self.env.insert(var.to_string(), v);
            }
            None => {
                self.env.remove(var);
            }
        }
        match saved_const {
            Some(c) => {
                self.const_env.insert(var.to_string(), c);
            }
            None => {
                self.const_env.remove(var);
            }
        }
        Ok(())
    }

    /// Lower an assignment, rebinding the target local (SSA) in `env`. For field
    /// / index places, the enclosing aggregate is read, the slice is replaced,
    /// and a fresh aggregate net is rebound.
    fn lower_assign(&mut self, place: &ir::Place, value: &ir::Expr) -> Result<(), String> {
        let rhs = self.lower_expr(value)?;
        let (root, new_net) = self.rebuild_place(place, rhs)?;
        self.env.insert(root, new_net);
        Ok(())
    }

    /// Compute the new net for the **root local** of `place` after writing `rhs`
    /// into the addressed slice. Returns `(root_name, new_root_net)`.
    fn rebuild_place(&mut self, place: &ir::Place, rhs: Net) -> Result<(String, Net), String> {
        match place {
            ir::Place::Local { name, .. } => Ok((name.clone(), rhs)),
            ir::Place::Field { base, index, .. } => {
                let base_ty = base.ty();
                let sname = match base_ty {
                    Type::Struct(n) => n,
                    _ => return Err("field assignment on non-struct".to_string()),
                };
                let def = self
                    .module
                    .structs
                    .get(sname)
                    .ok_or_else(|| format!("unknown struct `{}`", sname))?;
                let base_net = self.load_place(base)?;
                let off = struct_field_offset(def, *index, self.module)?;
                let fwidth = ty_width(&def.fields[*index].ty, self.module)?;
                let new_base =
                    self.replace_slice(&base_net, off, fwidth, &rhs);
                self.rebuild_place(base, new_base)
            }
            ir::Place::Index { base, index, .. } => {
                let (ew, _len) = match base.ty() {
                    Type::Array(elem, n) => (ty_width(elem, self.module)?, *n),
                    _ => return Err("index assignment on non-array".to_string()),
                };
                let idx = self
                    .const_eval(index)
                    .ok_or("dynamic array index not supported")?;
                if idx < 0 {
                    return Err("negative array index".to_string());
                }
                let off = (idx as u32) * ew;
                let base_net = self.load_place(base)?;
                let new_base = self.replace_slice(&base_net, off, ew, &rhs);
                self.rebuild_place(base, new_base)
            }
            ir::Place::TupleField { base, index, .. } => {
                let ts = match base.ty() {
                    Type::Tuple(ts) => ts.clone(),
                    _ => return Err("tuple assignment on non-tuple".to_string()),
                };
                let base_net = self.load_place(base)?;
                let off = tuple_elem_offset(&ts, *index, self.module)?;
                let fwidth = ty_width(&ts[*index], self.module)?;
                let new_base = self.replace_slice(&base_net, off, fwidth, &rhs);
                self.rebuild_place(base, new_base)
            }
        }
    }

    /// Read the current net for a place (root local read-modify-write support).
    fn load_place(&mut self, place: &ir::Place) -> Result<Net, String> {
        match place {
            ir::Place::Local { name, .. } => self
                .env
                .get(name)
                .cloned()
                .ok_or_else(|| format!("assignment to unbound local `{}`", name)),
            ir::Place::Field { base, index, .. } => {
                let base_net = self.load_place(base)?;
                let sname = match base.ty() {
                    Type::Struct(n) => n,
                    _ => return Err("field access on non-struct".to_string()),
                };
                let def = self
                    .module
                    .structs
                    .get(sname)
                    .ok_or_else(|| format!("unknown struct `{}`", sname))?;
                let off = struct_field_offset(def, *index, self.module)?;
                let fwidth = ty_width(&def.fields[*index].ty, self.module)?;
                Ok(self.slice(&base_net, off, fwidth))
            }
            ir::Place::Index { base, index, .. } => {
                let ew = match base.ty() {
                    Type::Array(elem, _) => ty_width(elem, self.module)?,
                    _ => return Err("index of non-array".to_string()),
                };
                let idx = self
                    .const_eval(index)
                    .ok_or("dynamic array index not supported")?;
                let base_net = self.load_place(base)?;
                Ok(self.slice(&base_net, (idx as u32) * ew, ew))
            }
            ir::Place::TupleField { base, index, .. } => {
                let ts = match base.ty() {
                    Type::Tuple(ts) => ts.clone(),
                    _ => return Err("tuple access on non-tuple".to_string()),
                };
                let base_net = self.load_place(base)?;
                let off = tuple_elem_offset(&ts, *index, self.module)?;
                let fwidth = ty_width(&ts[*index], self.module)?;
                Ok(self.slice(&base_net, off, fwidth))
            }
        }
    }

    /// Emit a sized wire holding `net[off+width-1 : off]`.
    fn slice(&mut self, net: &Net, off: u32, width: u32) -> Net {
        self.emit(
            width,
            Expr::Slice {
                e: Box::new(net.expr.clone()),
                hi: off + width - 1,
                lo: off,
            },
        )
    }

    /// Rebuild `base` with `[off+width-1 : off]` replaced by `rhs`, by
    /// concatenating the unchanged high part, the new value, and the unchanged
    /// low part. Returns a fresh sized wire of the same width as `base`.
    fn replace_slice(&mut self, base: &Net, off: u32, width: u32, rhs: &Net) -> Net {
        let total = base.width;
        let mut parts: Vec<(Expr, u32)> = Vec::new();
        // High part: [total-1 : off+width]
        if off + width < total {
            let hw = total - (off + width);
            parts.push((
                Expr::Slice {
                    e: Box::new(base.expr.clone()),
                    hi: total - 1,
                    lo: off + width,
                },
                hw,
            ));
        }
        // The replacement.
        parts.push((rhs.expr.clone(), width));
        // Low part: [off-1 : 0]
        if off > 0 {
            parts.push((
                Expr::Slice {
                    e: Box::new(base.expr.clone()),
                    hi: off - 1,
                    lo: 0,
                },
                off,
            ));
        }
        if parts.len() == 1 {
            // Whole-value replacement.
            self.emit(total, parts.pop().unwrap().0)
        } else {
            self.emit(total, Expr::Concat(parts))
        }
    }

    /// Lower an `if` used as a statement: lower each branch against a cloned env,
    /// then for every local whose net differs, emit a `cond ? then : else` mux.
    fn lower_if_stmt(
        &mut self,
        cond: &ir::Expr,
        then_branch: &ir::Block,
        else_branch: &ir::Block,
    ) -> Result<(), String> {
        let c = self.lower_expr(cond)?;

        let before = self.env.clone();
        // Then branch.
        let mut then_intro = Vec::new();
        self.lower_effect_block(then_branch, &mut then_intro)?;
        let then_env = self.env.clone();

        // Reset to `before`, lower the else branch.
        self.env = before.clone();
        let mut else_intro = Vec::new();
        self.lower_effect_block(else_branch, &mut else_intro)?;
        let else_env = self.env.clone();

        // Merge: for every name present before the branch, mux differing nets.
        self.env = before.clone();
        self.merge_envs(&c, &before, &then_env, &else_env)?;
        Ok(())
    }

    /// Lower a `match` used as a statement (unit-typed body), threading mutable
    /// assignments out through a phi merge across the arms.
    fn lower_match_stmt(
        &mut self,
        scrutinee: &ir::Expr,
        arms: &[ir::Arm],
    ) -> Result<(), String> {
        let scrut = self.lower_expr(scrutinee)?;
        let before = self.env.clone();

        // Lower each arm's effect into its own post-env plus a guard condition.
        // Build a ternary chain (last arm = fallthrough) per mutated name.
        struct ArmEffect {
            cond: Option<Expr>, // None == always (wildcard / irrefutable)
            env: HashMap<String, Net>,
        }
        let mut effects: Vec<ArmEffect> = Vec::new();

        let is_enum = matches!(scrutinee.ty, Type::Enum(_));
        let (tag_bits, payload_bits) = if is_enum {
            let ename = match &scrutinee.ty {
                Type::Enum(n) => n,
                _ => unreachable!(),
            };
            let def = self
                .module
                .enums
                .get(ename)
                .ok_or_else(|| format!("unknown enum `{}`", ename))?;
            (enum_tag_bits(def), enum_payload_bits(def, self.module)?)
        } else {
            (0, 0)
        };

        for arm in arms {
            self.env = before.clone();
            let cond =
                self.bind_pattern(&arm.pattern, &scrut, tag_bits, payload_bits, is_enum)?;
            // Lower the arm body for effect (unit-typed).
            self.lower_unit_expr(&arm.body)?;
            effects.push(ArmEffect {
                cond,
                env: self.env.clone(),
            });
        }

        // Merge per mutated name, last arm first as the fallthrough.
        self.env = before.clone();
        let names: Vec<String> = before.keys().cloned().collect();
        for name in names {
            let base = before.get(&name).cloned().unwrap();
            // Build from the last arm backwards.
            let mut acc: Option<Net> = None;
            for eff in effects.iter().rev() {
                let arm_net = eff.env.get(&name).cloned().unwrap_or_else(|| base.clone());
                acc = Some(match (&eff.cond, acc) {
                    (None, _) => arm_net, // irrefutable arm dominates the tail
                    (Some(_), None) => arm_net, // last arm fallthrough
                    (Some(c), Some(rest)) => self.emit(
                        arm_net.width,
                        Expr::Ternary {
                            c: Box::new(c.clone()),
                            t: Box::new(arm_net.expr),
                            e: Box::new(rest.expr),
                        },
                    ),
                });
            }
            if let Some(n) = acc {
                self.env.insert(name, n);
            }
        }
        Ok(())
    }

    /// Mux differing nets after an `if` statement: for each name in `before`,
    /// if its net differs between `then_env` and `else_env`, bind a ternary.
    fn merge_envs(
        &mut self,
        cond: &Net,
        before: &HashMap<String, Net>,
        then_env: &HashMap<String, Net>,
        else_env: &HashMap<String, Net>,
    ) -> Result<(), String> {
        let names: Vec<String> = before.keys().cloned().collect();
        for name in names {
            let t = then_env.get(&name);
            let e = else_env.get(&name);
            if let (Some(t), Some(e)) = (t, e) {
                if t.expr != e.expr {
                    let merged = self.emit(
                        t.width,
                        Expr::Ternary {
                            c: Box::new(cond.expr.clone()),
                            t: Box::new(t.expr.clone()),
                            e: Box::new(e.expr.clone()),
                        },
                    );
                    self.env.insert(name, merged);
                } else {
                    self.env.insert(name, t.clone());
                }
            }
        }
        Ok(())
    }

    /// Try to constant-fold an integer-like expression to an `i128`. Supports
    /// literals, bound loop variables, and `+ - * unary` over them — enough for
    /// loop bounds and array indices that reference loop counters.
    fn const_eval(&self, e: &ir::Expr) -> Option<i128> {
        match &e.kind {
            ir::ExprKind::Int(v) => Some(*v),
            ir::ExprKind::Bool(b) => Some(*b as i128),
            ir::ExprKind::Local(name) => self.const_env.get(name).copied(),
            ir::ExprKind::Unary { op, expr } => {
                let v = self.const_eval(expr)?;
                Some(match op {
                    UnOp::Neg => v.wrapping_neg(),
                    UnOp::Not => !v,
                })
            }
            ir::ExprKind::Binary { op, lhs, rhs } => {
                let a = self.const_eval(lhs)?;
                let b = self.const_eval(rhs)?;
                Some(match op {
                    BinOp::Add => a.wrapping_add(b),
                    BinOp::Sub => a.wrapping_sub(b),
                    BinOp::Mul => a.wrapping_mul(b),
                    BinOp::Div if b != 0 => a / b,
                    BinOp::Rem if b != 0 => a % b,
                    _ => return None,
                })
            }
            _ => None,
        }
    }

    fn lower_expr(&mut self, e: &ir::Expr) -> Result<Net, String> {
        match &e.kind {
            ir::ExprKind::Int(v) => {
                let width = ty_width(&e.ty, self.module)?;
                Ok(Net {
                    expr: Expr::Const {
                        value: (*v as u128) & mask(width),
                        width,
                    },
                    width,
                })
            }
            ir::ExprKind::Bool(b) => Ok(Net {
                expr: Expr::Const {
                    value: *b as u128,
                    width: 1,
                },
                width: 1,
            }),
            ir::ExprKind::Unit => Err("unit value is not lowerable".to_string()),
            ir::ExprKind::Local(name) => self
                .env
                .get(name)
                .cloned()
                .ok_or_else(|| format!("unbound local `{}`", name)),
            ir::ExprKind::Unary { op, expr } => {
                let inner = self.lower_expr(expr)?;
                let width = ty_width(&e.ty, self.module)?;
                Ok(self.emit(
                    width,
                    Expr::Un {
                        op: *op,
                        e: Box::new(inner.expr),
                    },
                ))
            }
            ir::ExprKind::Binary { op, lhs, rhs } => self.lower_binary(*op, lhs, rhs, &e.ty),
            ir::ExprKind::Call { func, args } => self.lower_call(func, args, &e.ty),
            ir::ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let c = self.lower_expr(cond)?;
                let t = self.lower_block(then_branch)?;
                let el = self.lower_block(else_branch)?;
                let width = ty_width(&e.ty, self.module)?;
                Ok(self.emit(
                    width,
                    Expr::Ternary {
                        c: Box::new(c.expr),
                        t: Box::new(t.expr),
                        e: Box::new(el.expr),
                    },
                ))
            }
            ir::ExprKind::Match { scrutinee, arms } => self.lower_match(scrutinee, arms, &e.ty),
            ir::ExprKind::Block(b) => self.lower_block(b),
            ir::ExprKind::Print { .. } => Err("`print` is not synthesizable".to_string()),
            ir::ExprKind::MakeStruct { fields, .. } => self.lower_make_struct(fields, &e.ty),
            ir::ExprKind::MakeEnum { tag, args, .. } => self.lower_make_enum(*tag, args, &e.ty),
            ir::ExprKind::Field { base, index } => self.lower_field(base, *index),
            ir::ExprKind::Index { base, index } => self.lower_index(base, index),
            ir::ExprKind::Array(elems) => self.lower_array(elems, &e.ty),
            // Tuples pack exactly like structs (element 0 in the LOW bits).
            ir::ExprKind::MakeTuple(elems) => self.lower_make_struct(elems, &e.ty),
            ir::ExprKind::TupleField { base, index } => self.lower_tuple_field(base, *index),
        }
    }

    fn lower_tuple_field(&mut self, base: &ir::Expr, index: usize) -> Result<Net, String> {
        let ts = match &base.ty {
            Type::Tuple(ts) => ts.clone(),
            _ => return Err("tuple access on non-tuple".to_string()),
        };
        let off = tuple_elem_offset(&ts, index, self.module)?;
        let fwidth = ty_width(&ts[index], self.module)?;
        let base_net = self.lower_expr(base)?;
        Ok(self.slice(&base_net, off, fwidth))
    }

    /// Build a struct value: concat field nets with field 0 in the LOW bits
    /// (so the concat list is reversed — highest field first).
    fn lower_make_struct(&mut self, fields: &[ir::Expr], ty: &Type) -> Result<Net, String> {
        let width = ty_width(ty, self.module)?;
        let mut nets = Vec::with_capacity(fields.len());
        for f in fields {
            nets.push(self.lower_expr(f)?);
        }
        Ok(self.pack_low_first(&nets, width))
    }

    /// Build an array value: concat element nets with element 0 in the LOW bits.
    fn lower_array(&mut self, elems: &[ir::Expr], ty: &Type) -> Result<Net, String> {
        let width = ty_width(ty, self.module)?;
        let mut nets = Vec::with_capacity(elems.len());
        for el in elems {
            nets.push(self.lower_expr(el)?);
        }
        if nets.is_empty() {
            // Zero-width arrays cannot exist as ports, but be defensive.
            return Ok(self.emit(width.max(1), Expr::Const { value: 0, width: width.max(1) }));
        }
        Ok(self.pack_low_first(&nets, width))
    }

    /// Concatenate `nets` so that `nets[0]` occupies the LOW bits and the last
    /// element the HIGH bits, materialised as a sized wire of `total` bits.
    fn pack_low_first(&mut self, nets: &[Net], total: u32) -> Net {
        if nets.len() == 1 && nets[0].width == total {
            return nets[0].clone();
        }
        // Concat lists are high-to-low, so reverse the low-first net order.
        let parts: Vec<(Expr, u32)> = nets
            .iter()
            .rev()
            .map(|n| (n.expr.clone(), n.width))
            .collect();
        self.emit(total, Expr::Concat(parts))
    }

    /// Build an enum value: `(tag << payload_bits) | payload`, where `payload`
    /// is the args packed field-0-low, zero-extended to `payload_bits`.
    fn lower_make_enum(&mut self, tag: usize, args: &[ir::Expr], ty: &Type) -> Result<Net, String> {
        let ename = match ty {
            Type::Enum(n) => n,
            _ => return Err("enum construction on non-enum type".to_string()),
        };
        let def = self
            .module
            .enums
            .get(ename)
            .ok_or_else(|| format!("unknown enum `{}`", ename))?;
        let tag_bits = enum_tag_bits(def);
        let payload_bits = enum_payload_bits(def, self.module)?;
        let total = tag_bits + payload_bits;

        // Pack the payload (args field-0-low) into `payload_bits` bits.
        let mut arg_nets = Vec::with_capacity(args.len());
        for a in args {
            arg_nets.push(self.lower_expr(a)?);
        }
        let payload = if payload_bits == 0 {
            None
        } else {
            let used: u32 = arg_nets.iter().map(|n| n.width).sum();
            let mut parts: Vec<(Expr, u32)> = Vec::new();
            // Zero-extend the high padding.
            if used < payload_bits {
                parts.push((
                    Expr::Const {
                        value: 0,
                        width: payload_bits - used,
                    },
                    payload_bits - used,
                ));
            }
            // Args high-to-low (field 0 lowest).
            for n in arg_nets.iter().rev() {
                parts.push((n.expr.clone(), n.width));
            }
            if parts.is_empty() {
                Some(self.emit(payload_bits, Expr::Const { value: 0, width: payload_bits }))
            } else if parts.len() == 1 && parts[0].1 == payload_bits {
                Some(self.emit(payload_bits, parts.pop().unwrap().0))
            } else {
                Some(self.emit(payload_bits, Expr::Concat(parts)))
            }
        };

        // Prepend the tag in the high bits.
        if tag_bits == 0 {
            // Single-variant enum: value is just the payload (or zero-width).
            return Ok(match payload {
                Some(p) => p,
                None => self.emit(total.max(1), Expr::Const { value: 0, width: total.max(1) }),
            });
        }
        let tag_net = self.emit(
            tag_bits,
            Expr::Const {
                value: tag as u128,
                width: tag_bits,
            },
        );
        match payload {
            Some(p) => Ok(self.emit(
                total,
                Expr::Concat(vec![(tag_net.expr, tag_bits), (p.expr, payload_bits)]),
            )),
            None => Ok(tag_net),
        }
    }

    /// Read struct field `index`: slice `[off+w-1 : off]` of the base net.
    fn lower_field(&mut self, base: &ir::Expr, index: usize) -> Result<Net, String> {
        let sname = match &base.ty {
            Type::Struct(n) => n,
            _ => return Err("field access on non-struct".to_string()),
        };
        let def = self
            .module
            .structs
            .get(sname)
            .ok_or_else(|| format!("unknown struct `{}`", sname))?;
        let off = struct_field_offset(def, index, self.module)?;
        let fwidth = ty_width(&def.fields[index].ty, self.module)?;
        let base_net = self.lower_expr(base)?;
        Ok(self.slice(&base_net, off, fwidth))
    }

    /// Read array element at a **constant** index: slice `[i*ew+ew-1 : i*ew]`.
    fn lower_index(&mut self, base: &ir::Expr, index: &ir::Expr) -> Result<Net, String> {
        let (ew, len) = match &base.ty {
            Type::Array(elem, n) => (ty_width(elem, self.module)?, *n),
            _ => return Err("indexing a non-array value".to_string()),
        };
        let idx = self
            .const_eval(index)
            .ok_or("dynamic array index not supported")?;
        if idx < 0 || (idx as u64) >= len {
            return Err(format!("constant array index {} out of bounds", idx));
        }
        let base_net = self.lower_expr(base)?;
        Ok(self.slice(&base_net, (idx as u32) * ew, ew))
    }

    fn lower_binary(
        &mut self,
        op: BinOp,
        lhs: &ir::Expr,
        rhs: &ir::Expr,
        ty: &Type,
    ) -> Result<Net, String> {
        let l = self.lower_expr(lhs)?;
        // Shifts: reduce the amount modulo the operand width (mask for power-of-two)
        // to mirror the interpreter's total shifts.
        if matches!(op, BinOp::Shl | BinOp::Shr) {
            let r = self.lower_expr(rhs)?;
            let w = l.width;
            let amt = self.reduce_shift_amount(r, w);
            return Ok(self.emit(
                w,
                Expr::Bin {
                    op,
                    l: Box::new(l.expr),
                    r: Box::new(amt.expr),
                },
            ));
        }

        let r = self.lower_expr(rhs)?;
        if op.is_comparison() {
            // Comparisons yield a 1-bit result.
            return Ok(self.emit(
                1,
                Expr::Bin {
                    op,
                    l: Box::new(l.expr),
                    r: Box::new(r.expr),
                },
            ));
        }
        if op.is_logical() {
            return Ok(self.emit(
                1,
                Expr::Bin {
                    op,
                    l: Box::new(l.expr),
                    r: Box::new(r.expr),
                },
            ));
        }
        // Arithmetic / bitwise: result width is the operand width.
        let width = ty_width(ty, self.module)?;
        Ok(self.emit(
            width,
            Expr::Bin {
                op,
                l: Box::new(l.expr),
                r: Box::new(r.expr),
            },
        ))
    }

    /// `amt mod width`, expressed as a mask for power-of-two widths or `%` otherwise.
    fn reduce_shift_amount(&mut self, amt: Net, width: u32) -> Net {
        if width.is_power_of_two() {
            let m = (width as u128) - 1;
            self.emit(
                amt.width,
                Expr::Bin {
                    op: BinOp::BitAnd,
                    l: Box::new(amt.expr),
                    r: Box::new(Expr::Const {
                        value: m,
                        width: amt.width,
                    }),
                },
            )
        } else {
            self.emit(
                amt.width,
                Expr::Bin {
                    op: BinOp::Rem,
                    l: Box::new(amt.expr),
                    r: Box::new(Expr::Const {
                        value: width as u128,
                        width: amt.width,
                    }),
                },
            )
        }
    }

    fn lower_call(&mut self, func: &str, args: &[ir::Expr], ty: &Type) -> Result<Net, String> {
        let callee = self
            .module
            .funcs
            .get(func)
            .ok_or_else(|| format!("call to unknown function `{}`", func))?;
        let out_width = ty_width(ty, self.module)?;
        let mut conns = Vec::new();
        for (p, a) in callee.params.iter().zip(args.iter()) {
            let net = self.lower_expr(a)?;
            conns.push((p.name.clone(), net.expr));
        }
        let out_wire = self.fresh("call");
        let inst = self.fresh("u");
        self.items.push(Item::Instance {
            module: sanitize(func),
            inst,
            conns,
            out_wire: out_wire.clone(),
            out_width,
        });
        Ok(Net {
            expr: Expr::Ref(out_wire),
            width: out_width,
        })
    }

    /// Lower a value-position `match`. Scalar (`bit<N>`/`bool`) and enum/variant
    /// scrutinees are both supported: each arm yields a guard condition (or no
    /// guard for an irrefutable arm) and a body lowered with its bindings
    /// installed; the arms fold into a ternary chain (earlier arms win).
    fn lower_match(
        &mut self,
        scrutinee: &ir::Expr,
        arms: &[ir::Arm],
        ty: &Type,
    ) -> Result<Net, String> {
        let scrut = self.lower_expr(scrutinee)?;
        let width = ty_width(ty, self.module)?;

        let is_enum = matches!(scrutinee.ty, Type::Enum(_));
        let (tag_bits, payload_bits) = if is_enum {
            let ename = match &scrutinee.ty {
                Type::Enum(n) => n,
                _ => unreachable!(),
            };
            let def = self
                .module
                .enums
                .get(ename)
                .ok_or_else(|| format!("unknown enum `{}`", ename))?;
            (enum_tag_bits(def), enum_payload_bits(def, self.module)?)
        } else {
            (0, 0)
        };

        // Build from the last arm backwards so earlier arms take priority.
        let mut acc: Option<Net> = None;
        for arm in arms.iter().rev() {
            let saved = self.env.clone();
            let cond =
                self.bind_pattern(&arm.pattern, &scrut, tag_bits, payload_bits, is_enum)?;
            let body = self.lower_expr(&arm.body)?;
            self.env = saved;

            acc = Some(match (cond, acc) {
                (None, _) => body, // irrefutable: dominates the tail
                (Some(_), None) => body, // last arm acts as fallthrough
                (Some(c), Some(rest)) => self.emit(
                    width,
                    Expr::Ternary {
                        c: Box::new(c),
                        t: Box::new(body.expr),
                        e: Box::new(rest.expr),
                    },
                ),
            });
        }
        acc.ok_or_else(|| "empty match".to_string())
    }

    /// Install the bindings a pattern introduces into `env` (caller restores the
    /// env afterwards) and return the guard condition expression, or `None` if
    /// the pattern is irrefutable. `scrut` is the scrutinee net; for enum
    /// scrutinees, `tag_bits`/`payload_bits` describe the packed layout.
    fn bind_pattern(
        &mut self,
        pat: &ir::Pattern,
        scrut: &Net,
        tag_bits: u32,
        payload_bits: u32,
        is_enum: bool,
    ) -> Result<Option<Expr>, String> {
        match pat {
            ir::Pattern::Wildcard => Ok(None),
            ir::Pattern::Binding { name, .. } => {
                self.env.insert(name.clone(), scrut.clone());
                Ok(None)
            }
            ir::Pattern::Int(v) => {
                let cmp = self.emit(
                    1,
                    Expr::Bin {
                        op: BinOp::Eq,
                        l: Box::new(scrut.expr.clone()),
                        r: Box::new(Expr::Const {
                            value: (*v as u128) & mask(scrut.width),
                            width: scrut.width,
                        }),
                    },
                );
                Ok(Some(cmp.expr))
            }
            ir::Pattern::Bool(b) => {
                let cmp = self.emit(
                    1,
                    Expr::Bin {
                        op: BinOp::Eq,
                        l: Box::new(scrut.expr.clone()),
                        r: Box::new(Expr::Const {
                            value: *b as u128,
                            width: 1,
                        }),
                    },
                );
                Ok(Some(cmp.expr))
            }
            ir::Pattern::Variant {
                enum_name,
                tag,
                subpatterns,
            } => {
                if !is_enum {
                    return Err("variant pattern on non-enum scrutinee".to_string());
                }
                let def = self
                    .module
                    .enums
                    .get(enum_name)
                    .ok_or_else(|| format!("unknown enum `{}`", enum_name))?;
                // Tag comparison (high bits). With a single variant, tag_bits == 0
                // and the tag match is unconditional.
                let mut cond: Option<Expr> = if tag_bits == 0 {
                    None
                } else {
                    let tag_net = self.slice(scrut, payload_bits, tag_bits);
                    let cmp = self.emit(
                        1,
                        Expr::Bin {
                            op: BinOp::Eq,
                            l: Box::new(tag_net.expr),
                            r: Box::new(Expr::Const {
                                value: *tag as u128,
                                width: tag_bits,
                            }),
                        },
                    );
                    Some(cmp.expr)
                };

                // Bind each subpattern to its payload slice (field-0-low).
                let field_tys = &def.variants[*tag].fields;
                let mut off = 0u32;
                for (sp, fty) in subpatterns.iter().zip(field_tys.iter()) {
                    let fw = ty_width(fty, self.module)?;
                    let field_net = self.slice(scrut, off, fw);
                    // Recurse: subpatterns are matched against the field net.
                    // Nested enums are not expected here, but handle scalar
                    // literal/binding/wildcard subpatterns.
                    let sub_cond = self.bind_pattern(sp, &field_net, 0, 0, false)?;
                    if let Some(sc) = sub_cond {
                        cond = Some(match cond.take() {
                            Some(c) => {
                                let anded = self.emit(
                                    1,
                                    Expr::Bin {
                                        op: BinOp::And,
                                        l: Box::new(c),
                                        r: Box::new(sc),
                                    },
                                );
                                anded.expr
                            }
                            None => sc,
                        });
                    }
                    off += fw;
                }
                Ok(cond)
            }
            ir::Pattern::Range { lo, hi, inclusive } => {
                let ge = self.emit(
                    1,
                    Expr::Bin {
                        op: BinOp::Ge,
                        l: Box::new(scrut.expr.clone()),
                        r: Box::new(Expr::Const {
                            value: (*lo as u128) & mask(scrut.width),
                            width: scrut.width,
                        }),
                    },
                );
                let hi_op = if *inclusive { BinOp::Le } else { BinOp::Lt };
                let le = self.emit(
                    1,
                    Expr::Bin {
                        op: hi_op,
                        l: Box::new(scrut.expr.clone()),
                        r: Box::new(Expr::Const {
                            value: (*hi as u128) & mask(scrut.width),
                            width: scrut.width,
                        }),
                    },
                );
                let both = self.emit(
                    1,
                    Expr::Bin {
                        op: BinOp::And,
                        l: Box::new(ge.expr),
                        r: Box::new(le.expr),
                    },
                );
                Ok(Some(both.expr))
            }
            // Or-pattern alternatives are binding-free (enforced by the
            // typechecker), so the condition is the OR of the alternatives'.
            ir::Pattern::Or(alts) => {
                let mut acc: Option<Expr> = None;
                let mut irrefutable = false;
                for alt in alts {
                    match self.bind_pattern(alt, scrut, tag_bits, payload_bits, is_enum)? {
                        None => irrefutable = true,
                        Some(c) => {
                            acc = Some(match acc.take() {
                                Some(a) => {
                                    self.emit(
                                        1,
                                        Expr::Bin {
                                            op: BinOp::Or,
                                            l: Box::new(a),
                                            r: Box::new(c),
                                        },
                                    )
                                    .expr
                                }
                                None => c,
                            });
                        }
                    }
                }
                if irrefutable {
                    Ok(None)
                } else {
                    Ok(acc)
                }
            }
            ir::Pattern::Tuple(_) => {
                Err("tuple patterns in `match` are not yet supported by codegen".to_string())
            }
        }
    }
}

fn mask(width: u32) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

// ===================================================================
// Reference evaluator — a bit-exact Verilog-semantics interpreter used to
// prove equivalence with the Rune interpreter when no HDL simulator is present.
// ===================================================================

/// Evaluate a generated [`Design`]'s module `name` on the given input port
/// values, returning the value driven onto `out`. Mirrors Verilog semantics
/// (unsigned, each wire truncated to its declared width).
pub fn eval(design: &Design, name: &str, inputs: &HashMap<String, u128>) -> Option<u128> {
    let module = design.modules.iter().find(|m| m.name == name)?;
    let mut env: HashMap<String, u128> = HashMap::new();
    for p in module.input_ports() {
        let v = *inputs.get(&p.name)?;
        env.insert(p.name.clone(), v & mask(p.width));
    }
    for item in &module.items {
        match item {
            Item::Wire { name, width, value } => {
                let v = eval_expr(value, &env) & mask(*width);
                env.insert(name.clone(), v);
            }
            Item::Assign { lhs, rhs } => {
                let w = module.ports.iter().find(|p| &p.name == lhs).map(|p| p.width);
                let v = eval_expr(rhs, &env);
                let v = match w {
                    Some(w) => v & mask(w),
                    None => v,
                };
                env.insert(lhs.clone(), v);
            }
            Item::Instance {
                module: sub,
                conns,
                out_wire,
                out_width,
                ..
            } => {
                let mut sub_inputs = HashMap::new();
                // The submodule's input ports are named after its parameters; we
                // connected them by name in `conns`.
                for (port, expr) in conns {
                    sub_inputs.insert(port.clone(), eval_expr(expr, &env));
                }
                let v = eval(design, sub, &sub_inputs)?;
                env.insert(out_wire.clone(), v & mask(*out_width));
            }
        }
    }
    env.get("out").copied()
}

fn eval_expr(e: &Expr, env: &HashMap<String, u128>) -> u128 {
    match e {
        Expr::Const { value, width } => value & mask(*width),
        Expr::Ref(n) => *env.get(n).unwrap_or(&0),
        Expr::Un { op, e } => {
            let v = eval_expr(e, env);
            match op {
                // Width truncation happens at the enclosing wire; here, compute
                // the unbounded value and let the wire mask it.
                UnOp::Neg => v.wrapping_neg(),
                UnOp::Not => !v,
            }
        }
        Expr::Bin { op, l, r } => {
            let a = eval_expr(l, env);
            let b = eval_expr(r, env);
            eval_binop(*op, a, b)
        }
        Expr::Ternary { c, t, e } => {
            if eval_expr(c, env) != 0 {
                eval_expr(t, env)
            } else {
                eval_expr(e, env)
            }
        }
        Expr::Concat(parts) => {
            // The first part is most significant; shift each part in by its width.
            let mut acc: u128 = 0;
            for (p, w) in parts {
                acc = (acc << *w) | (eval_expr(p, env) & mask(*w));
            }
            acc
        }
        Expr::Slice { e, hi, lo } => {
            let v = eval_expr(e, env);
            let width = hi - lo + 1;
            (v >> lo) & mask(width)
        }
    }
}

fn eval_binop(op: BinOp, a: u128, b: u128) -> u128 {
    let bool_to = |x: bool| x as u128;
    match op {
        BinOp::Add => a.wrapping_add(b),
        BinOp::Sub => a.wrapping_sub(b),
        BinOp::Mul => a.wrapping_mul(b),
        BinOp::Div => {
            if b == 0 {
                0
            } else {
                a / b
            }
        }
        BinOp::Rem => {
            if b == 0 {
                0
            } else {
                a % b
            }
        }
        BinOp::BitAnd => a & b,
        BinOp::BitOr => a | b,
        BinOp::BitXor => a ^ b,
        BinOp::Shl => {
            if b >= 128 {
                0
            } else {
                a << b
            }
        }
        BinOp::Shr => {
            if b >= 128 {
                0
            } else {
                a >> b
            }
        }
        BinOp::And => bool_to(a != 0 && b != 0),
        BinOp::Or => bool_to(a != 0 || b != 0),
        BinOp::Eq => bool_to(a == b),
        BinOp::Ne => bool_to(a != b),
        BinOp::Lt => bool_to(a < b),
        BinOp::Le => bool_to(a <= b),
        BinOp::Gt => bool_to(a > b),
        BinOp::Ge => bool_to(a >= b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::{Interpreter, Value};
    use crate::ir::IntTy;

    fn compile(src: &str) -> ir::Module {
        crate::compile(src).expect("typechecks")
    }

    /// Check that the lowered Verilog netlist computes the same value as the
    /// interpreter for `func` over a set of `bit<width>` input tuples.
    fn assert_equiv(src: &str, func: &str, width: u32, cases: &[Vec<u128>]) {
        let module = compile(src);
        let design = lower(&module).design;
        let vmod = sanitize(func);

        for case in cases {
            // Interpreter result.
            let args: Vec<Value> = case.iter().map(|&v| Value::Bit(v & mask(width), width)).collect();
            let mut interp = Interpreter::new(module.clone());
            let got = interp.call(func, args).expect("interp runs");
            let expected = match got {
                Value::Bit(v, _) => v,
                Value::Bool(b) => b as u128,
                other => panic!("unexpected interp value {:?}", other),
            };

            // Netlist result.
            let ir_func = &module.funcs[func];
            let mut inputs = HashMap::new();
            for (p, v) in ir_func.params.iter().zip(case.iter()) {
                inputs.insert(p.name.clone(), *v & mask(width));
            }
            let actual = eval(&design, &vmod, &inputs).expect("eval netlist");
            assert_eq!(
                actual, expected,
                "mismatch for {}({:?}): netlist {} vs interp {}",
                func, case, actual, expected
            );
        }
    }

    #[test]
    fn add8_lowers_and_matches() {
        let src = "fn add8(a: bit<8>, b: bit<8>) -> bit<8> { a + b }";
        // Exhaustive-ish sampling incl. the wrapping case 200+100=44.
        let cases: Vec<Vec<u128>> = vec![
            vec![200, 100],
            vec![255, 1],
            vec![0, 0],
            vec![17, 25],
            vec![128, 128],
        ];
        assert_equiv(src, "add8", 8, &cases);

        // The emitted module looks right.
        let m = compile(src);
        let text = emit(&m);
        assert!(text.contains("module add8"));
        assert!(text.contains("input [7:0] a"));
        assert!(text.contains("output [7:0] out"));
    }

    #[test]
    fn bit_ops_match_interpreter() {
        let src = r#"
            fn rotl4(x: bit<8>, n: bit<8>) -> bit<8> {
                let m: bit<8> = n & 7;
                (x << m) | (x >> ((8 - m) & 7))
            }
            fn mask_low(x: bit<8>, i: bit<8>) -> bit<8> { (x >> (i & 7)) & 1 }
        "#;
        let mut cases = Vec::new();
        for x in [0u128, 1, 0x80, 0xA5, 0xFF] {
            for n in 0u128..8 {
                cases.push(vec![x, n]);
            }
        }
        assert_equiv(src, "rotl4", 8, &cases);
        assert_equiv(src, "mask_low", 8, &cases);
    }

    #[test]
    fn nested_arithmetic_truncates_each_step() {
        // (a + b) * c wraps at every operation; the SSA lowering must match.
        let src = "fn f(a: bit<8>, b: bit<8>, c: bit<8>) -> bit<8> { (a + b) * c }";
        let cases: Vec<Vec<u128>> = vec![
            vec![200, 100, 3],
            vec![255, 255, 255],
            vec![16, 16, 16],
            vec![1, 2, 3],
        ];
        assert_equiv(src, "f", 8, &cases);
    }

    #[test]
    fn match_on_scalar_lowers() {
        let src = r#"
            fn pick(x: bit<8>) -> bit<8> {
                match x {
                    0 => 100,
                    1 => 200,
                    _ => 7,
                }
            }
        "#;
        assert_equiv(src, "pick", 8, &[vec![0], vec![1], vec![2], vec![255]]);
    }

    #[test]
    fn calls_become_instances() {
        let src = r#"
            fn inc(x: bit<8>) -> bit<8> { x + 1 }
            fn inc2(x: bit<8>) -> bit<8> { inc(inc(x)) }
        "#;
        assert_equiv(src, "inc2", 8, &[vec![0], vec![254], vec![255], vec![40]]);
        let text = emit(&compile(src));
        assert!(text.contains("inc u"), "should instantiate inc: {}", text);
    }

    #[test]
    fn non_synthesizable_is_skipped_not_lowered() {
        let src = r#"
            fn pure_add(a: bit<8>, b: bit<8>) -> bit<8> { a + b }
            fn uses_machine_int(a: u32) -> u32 { a + 1 }
        "#;
        let result = lower(&compile(src));
        assert!(result.design.modules.iter().any(|m| m.name == "pure_add"));
        assert!(result.skipped.iter().any(|(n, _)| n == "uses_machine_int"));
    }

    #[test]
    fn signed_machine_int_not_lowered() {
        // Sanity: `i32` width helper rejects machine integers.
        let empty = ir::Module::default();
        assert!(ty_width(&Type::Int(IntTy { signed: true, width: 32 }), &empty).is_err());
        assert_eq!(ty_width(&Type::Bit(12), &empty).unwrap(), 12);
        assert_eq!(ty_width(&Type::Bool, &empty).unwrap(), 1);
    }

    // ===============================================================
    // Aggregate / loop / mutable-local equivalence tests.
    // ===============================================================

    /// Pack an interpreter [`Value`] into the same `u128` bit layout the lowerer
    /// uses: struct = fields low-to-high, array = elements low-to-high,
    /// enum = `(tag << payload_bits) | payload`, bit/bool = the value.
    fn pack(v: &Value, ty: &Type, module: &ir::Module) -> u128 {
        match (v, ty) {
            (Value::Bool(b), _) => *b as u128,
            (Value::Bit(n, _), _) => *n & mask(ty_width(ty, module).unwrap()),
            (Value::Struct(_, fields), Type::Struct(name)) => {
                let def = &module.structs[name];
                let mut acc = 0u128;
                let mut off = 0u32;
                for (fv, fd) in fields.iter().zip(def.fields.iter()) {
                    let fw = ty_width(&fd.ty, module).unwrap();
                    acc |= pack(fv, &fd.ty, module) << off;
                    off += fw;
                }
                acc
            }
            (Value::Array(elems), Type::Array(elem, _)) => {
                let ew = ty_width(elem, module).unwrap();
                let mut acc = 0u128;
                let mut off = 0u32;
                for ev in elems {
                    acc |= pack(ev, elem, module) << off;
                    off += ew;
                }
                acc
            }
            (Value::Enum(_, tag, args), Type::Enum(name)) => {
                let def = &module.enums[name];
                let payload_bits = enum_payload_bits(def, module).unwrap();
                let variant = &def.variants[*tag];
                let mut payload = 0u128;
                let mut off = 0u32;
                for (av, at) in args.iter().zip(variant.fields.iter()) {
                    let aw = ty_width(at, module).unwrap();
                    payload |= pack(av, at, module) << off;
                    off += aw;
                }
                ((*tag as u128) << payload_bits) | payload
            }
            other => panic!("cannot pack {:?} as {:?}", other, ty),
        }
    }

    /// Build an interpreter argument [`Value`] for a hardware type from a packed
    /// `u128` (inverse of `pack` for inputs).
    fn unpack(bits: u128, ty: &Type, module: &ir::Module) -> Value {
        match ty {
            Type::Bool => Value::Bool(bits & 1 != 0),
            Type::Bit(n) => Value::Bit(bits & mask(*n), *n),
            Type::Array(elem, len) => {
                let ew = ty_width(elem, module).unwrap();
                let mut elems = Vec::new();
                for i in 0..*len {
                    let slice = (bits >> (i as u32 * ew)) & mask(ew);
                    elems.push(unpack(slice, elem, module));
                }
                Value::Array(elems)
            }
            Type::Struct(name) => {
                let def = &module.structs[name];
                let mut off = 0u32;
                let mut fields = Vec::new();
                for fd in &def.fields {
                    let fw = ty_width(&fd.ty, module).unwrap();
                    let slice = (bits >> off) & mask(fw);
                    fields.push(unpack(slice, &fd.ty, module));
                    off += fw;
                }
                Value::Struct(name.clone(), fields)
            }
            Type::Enum(name) => {
                let def = &module.enums[name];
                let payload_bits = enum_payload_bits(def, module).unwrap();
                let tag = if enum_tag_bits(def) == 0 {
                    0
                } else {
                    (bits >> payload_bits) as usize
                };
                let variant = &def.variants[tag];
                let mut off = 0u32;
                let mut args = Vec::new();
                for at in &variant.fields {
                    let aw = ty_width(at, module).unwrap();
                    let slice = (bits >> off) & mask(aw);
                    args.push(unpack(slice, at, module));
                    off += aw;
                }
                Value::Enum(name.clone(), tag, args)
            }
            other => panic!("cannot unpack type {:?}", other),
        }
    }

    /// General equivalence check over arbitrary hardware-typed args. Each case is
    /// a vector of packed `u128` inputs (one per parameter).
    fn assert_equiv_agg(src: &str, func: &str, cases: &[Vec<u128>]) {
        let module = compile(src);
        let result = lower(&module);
        assert!(
            result.design.modules.iter().any(|m| m.name == sanitize(func)),
            "function `{}` was not lowered: skipped = {:?}",
            func,
            result.skipped
        );
        let design = result.design;
        let vmod = sanitize(func);
        let ir_func = &module.funcs[func];

        for case in cases {
            // Interpreter result.
            let args: Vec<Value> = ir_func
                .params
                .iter()
                .zip(case.iter())
                .map(|(p, &bits)| unpack(bits, &p.ty, &module))
                .collect();
            let mut interp = Interpreter::new(module.clone());
            let got = interp.call(func, args).expect("interp runs");
            let expected = pack(&got, &ir_func.ret, &module);

            // Netlist result.
            let mut inputs = HashMap::new();
            for (p, &bits) in ir_func.params.iter().zip(case.iter()) {
                let w = ty_width(&p.ty, &module).unwrap();
                inputs.insert(p.name.clone(), bits & mask(w));
            }
            let actual = eval(&design, &vmod, &inputs).expect("eval netlist");
            assert_eq!(
                actual, expected,
                "mismatch for {}({:?}): netlist {} vs interp {}",
                func, case, actual, expected
            );
        }
    }

    #[test]
    fn struct_build_field_read_and_mutate() {
        let src = r#"
            struct Point { x: bit<8>, y: bit<8> }
            fn make(a: bit<8>, b: bit<8>) -> Point { Point { x: a, y: b } }
            fn getx(p: Point) -> bit<8> { p.x }
            fn gety(p: Point) -> bit<8> { p.y }
            fn swap_sum(a: bit<8>, b: bit<8>) -> bit<8> {
                let mut p: Point = Point { x: a, y: b };
                p.x = b;
                p.y = a;
                p.x + p.y
            }
        "#;
        // make: build a struct, packed (y high, x low).
        let cases = vec![
            vec![0u128, 0],
            vec![0x12, 0x34],
            vec![255, 1],
            vec![0xAB, 0xCD],
        ];
        assert_equiv_agg(src, "make", &cases);
        // getx / gety take a Point input.
        let pt_cases: Vec<Vec<u128>> = vec![
            vec![0x3412], // x=0x12, y=0x34
            vec![0x00FF],
            vec![0xCDAB],
        ];
        assert_equiv_agg(src, "getx", &pt_cases);
        assert_equiv_agg(src, "gety", &pt_cases);
        assert_equiv_agg(src, "swap_sum", &cases);
    }

    #[test]
    fn enum_match_extracts_payloads() {
        let src = r#"
            enum E { A(bit<8>), B(bit<8>, bit<8>) }
            fn mk_a(x: bit<8>) -> E { E::A(x) }
            fn mk_b(x: bit<8>, y: bit<8>) -> E { E::B(x, y) }
            fn sum(e: E) -> bit<8> {
                match e {
                    E::A(v) => v,
                    E::B(p, q) => p + q,
                }
            }
        "#;
        let module = compile(src);
        let etype = Type::Enum("E".to_string());
        // Construction equivalence.
        assert_equiv_agg(src, "mk_a", &[vec![0], vec![0x42], vec![255]]);
        assert_equiv_agg(src, "mk_b", &[vec![1, 2], vec![100, 200], vec![255, 255]]);

        // sum over both variants: build packed E values via pack().
        let mut cases = Vec::new();
        for &x in &[0u128, 5, 200, 255] {
            let v = Value::Enum("E".to_string(), 0, vec![Value::Bit(x, 8)]);
            cases.push(vec![pack(&v, &etype, &module)]);
        }
        for (p, q) in [(1u128, 2u128), (200, 100), (255, 255), (0, 0)] {
            let v = Value::Enum(
                "E".to_string(),
                1,
                vec![Value::Bit(p, 8), Value::Bit(q, 8)],
            );
            cases.push(vec![pack(&v, &etype, &module)]);
        }
        assert_equiv_agg(src, "sum", &cases);
    }

    #[test]
    fn array_build_index_and_for_accumulate() {
        let src = r#"
            fn build(a: bit<8>, b: bit<8>, c: bit<8>, d: bit<8>) -> [bit<8>; 4] {
                [a, b, c, d]
            }
            fn third(a: bit<8>, b: bit<8>, c: bit<8>, d: bit<8>) -> bit<8> {
                let arr: [bit<8>; 4] = [a, b, c, d];
                arr[2]
            }
            fn fold(arr: [bit<8>; 4]) -> bit<8> {
                let mut acc: bit<8> = 0;
                for i in 0..4 {
                    acc = acc + arr[i];
                }
                acc
            }
        "#;
        // build & third
        let cases = vec![
            vec![1u128, 2, 3, 4],
            vec![255, 1, 128, 64],
            vec![0, 0, 0, 0],
            vec![10, 20, 30, 40],
        ];
        assert_equiv_agg(src, "build", &cases);
        assert_equiv_agg(src, "third", &cases);

        // fold takes one packed array argument.
        let module = compile(src);
        let arr_ty = Type::Array(Box::new(Type::Bit(8)), 4);
        let mut fold_cases = Vec::new();
        for c in &[
            [1u128, 2, 3, 4],
            [200, 100, 50, 25],
            [255, 255, 255, 255],
            [0, 0, 0, 0],
        ] {
            let arr = Value::Array(c.iter().map(|&n| Value::Bit(n, 8)).collect());
            fold_cases.push(vec![pack(&arr, &arr_ty, &module)]);
        }
        assert_equiv_agg(src, "fold", &fold_cases);
    }

    #[test]
    fn if_assignment_phi_merge() {
        // `let mut m = a; if b > m { m = b; } m` — needs a phi merge.
        let src = r#"
            fn max2(a: bit<8>, b: bit<8>) -> bit<8> {
                let mut m: bit<8> = a;
                if b > m {
                    m = b;
                }
                m
            }
            fn clamp_lo(a: bit<8>) -> bit<8> {
                let mut x: bit<8> = a;
                if x < 16 {
                    x = 16;
                } else {
                    x = x - 1;
                }
                x
            }
        "#;
        // Exhaustive over a representative grid for max2.
        let mut cases = Vec::new();
        for a in [0u128, 1, 16, 17, 100, 200, 255] {
            for b in [0u128, 1, 16, 17, 100, 200, 255] {
                cases.push(vec![a, b]);
            }
        }
        assert_equiv_agg(src, "max2", &cases);
        // Exhaustive single 8-bit input for clamp_lo.
        let clamp_cases: Vec<Vec<u128>> = (0u128..256).map(|x| vec![x]).collect();
        assert_equiv_agg(src, "clamp_lo", &clamp_cases);
    }

    #[test]
    fn for_loop_with_inner_if_accumulator() {
        // Accumulator `for` with an `if` inside (phi merge across loop body).
        let src = r#"
            fn count_big(arr: [bit<8>; 4]) -> bit<8> {
                let mut n: bit<8> = 0;
                for i in 0..4 {
                    if arr[i] > 127 {
                        n = n + 1;
                    }
                }
                n
            }
        "#;
        let module = compile(src);
        let arr_ty = Type::Array(Box::new(Type::Bit(8)), 4);
        let mut cases = Vec::new();
        for c in &[
            [200u128, 10, 250, 5],
            [0, 0, 0, 0],
            [255, 255, 255, 255],
            [128, 127, 200, 50],
        ] {
            let arr = Value::Array(c.iter().map(|&n| Value::Bit(n, 8)).collect());
            cases.push(vec![pack(&arr, &arr_ty, &module)]);
        }
        assert_equiv_agg(src, "count_big", &cases);
    }

    #[test]
    fn aggregate_text_uses_concat_and_slice() {
        let src = r#"
            struct Point { x: bit<8>, y: bit<8> }
            fn build(a: bit<8>, b: bit<8>) -> Point { Point { x: a, y: b } }
            fn getx(p: Point) -> bit<8> { p.x }
        "#;
        let text = emit(&compile(src));
        assert!(text.contains("module build"), "build module: {}", text);
        assert!(text.contains('{'), "should emit a concat: {}", text);
        assert!(text.contains('['), "should emit a slice: {}", text);
    }
}

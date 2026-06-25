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
//! `if`/`match` as value expressions, and calls. Aggregate types
//! (`struct`/`enum`/array), `for`/`while`/assignment, and `return` are reported
//! as unsupported-for-codegen (distinct from "not synthesizable"), to be grown
//! in a later phase. **No codegen for anything the analysis pass rejects.**

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

/// The width of a *hardware* type, or `Err` if it is not lowerable in this phase.
fn ty_width(t: &Type) -> Result<u32, String> {
    match t {
        Type::Bool => Ok(1),
        Type::Bit(n) => Ok(*n),
        Type::Int(it) => Err(format!("machine integer `{}` is not a hardware type", it_name(it))),
        Type::Array(..) => Err("arrays are not yet supported by codegen".to_string()),
        Type::Struct(n) => Err(format!("struct `{}` not yet supported by codegen", n)),
        Type::Enum(n) => Err(format!("enum `{}` not yet supported by codegen", n)),
        Type::Unit => Err("unit type cannot be a hardware value".to_string()),
    }
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
    /// local name -> current net (immutable bindings + params).
    env: HashMap<String, Net>,
}

fn lower_func(func: &ir::Func, module: &ir::Module) -> Result<Module, String> {
    let mut ports = Vec::new();
    let mut lo = Lowerer {
        module,
        items: Vec::new(),
        temp: 0,
        env: HashMap::new(),
    };

    for p in &func.params {
        if p.name == "out" {
            return Err("a parameter named `out` collides with the output port".to_string());
        }
        let width = ty_width(&p.ty)?;
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
    let ret_width = ty_width(&func.ret)?;
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

    fn lower_block(&mut self, b: &ir::Block) -> Result<Net, String> {
        // Snapshot bindings so block-local `let`s don't leak (and shadows restore).
        let saved = self.env.clone();
        let mut result = None;
        for s in &b.stmts {
            match s {
                ir::Stmt::Let { name, init, .. } => {
                    let net = self.lower_expr(init)?;
                    // Give the binding a readable, namespaced wire.
                    let wname = self.fresh(&format!("v{}_", sanitize(name)));
                    self.items.push(Item::Wire {
                        name: wname.clone(),
                        width: net.width,
                        value: net.expr,
                    });
                    self.env.insert(
                        name.clone(),
                        Net {
                            expr: Expr::Ref(wname),
                            width: net.width,
                        },
                    );
                }
                ir::Stmt::Expr(_) => {
                    // Pure expression statement: no observable effect in hardware.
                }
                ir::Stmt::Return { value } => {
                    result = Some(match value {
                        Some(e) => self.lower_expr(e)?,
                        None => return Err("`return` of unit is not lowerable".to_string()),
                    });
                    break;
                }
                ir::Stmt::Assign { .. } => {
                    return Err("assignment / mutable locals not yet supported".to_string())
                }
                ir::Stmt::While { .. } => return Err("`while` loops are not synthesizable".to_string()),
                ir::Stmt::For { .. } => {
                    return Err("`for` loops (unrolling) not yet supported".to_string())
                }
            }
        }
        let net = match result {
            Some(n) => n,
            None => match &b.tail {
                Some(e) => self.lower_expr(e)?,
                None => return Err("block has no value to lower".to_string()),
            },
        };
        self.env = saved;
        Ok(net)
    }

    fn lower_expr(&mut self, e: &ir::Expr) -> Result<Net, String> {
        match &e.kind {
            ir::ExprKind::Int(v) => {
                let width = ty_width(&e.ty)?;
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
                let width = ty_width(&e.ty)?;
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
                let width = ty_width(&e.ty)?;
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
            ir::ExprKind::MakeStruct { .. } | ir::ExprKind::MakeEnum { .. } => {
                Err("aggregate construction not yet supported by codegen".to_string())
            }
            ir::ExprKind::Field { .. } | ir::ExprKind::Index { .. } | ir::ExprKind::Array(_) => {
                Err("aggregate access not yet supported by codegen".to_string())
            }
        }
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
        let width = ty_width(ty)?;
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
        let out_width = ty_width(ty)?;
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

    /// Lower `match` over a scalar (`bit<N>`/`bool`) scrutinee with literal,
    /// wildcard, and binding patterns into a chain of ternaries. Enum/variant
    /// patterns are deferred to a later phase.
    fn lower_match(
        &mut self,
        scrutinee: &ir::Expr,
        arms: &[ir::Arm],
        ty: &Type,
    ) -> Result<Net, String> {
        let scrut = self.lower_expr(scrutinee)?;
        let width = ty_width(ty)?;

        // Build from the last arm backwards so earlier arms take priority.
        let mut acc: Option<Net> = None;
        for arm in arms.iter().rev() {
            match &arm.pattern {
                ir::Pattern::Wildcard => {
                    acc = Some(self.lower_expr(&arm.body)?);
                }
                ir::Pattern::Binding { name, .. } => {
                    let prev = self.env.insert(name.clone(), scrut.clone());
                    let body = self.lower_expr(&arm.body)?;
                    match prev {
                        Some(p) => {
                            self.env.insert(name.clone(), p);
                        }
                        None => {
                            self.env.remove(name);
                        }
                    }
                    acc = Some(body);
                }
                ir::Pattern::Int(v) => {
                    let body = self.lower_expr(&arm.body)?;
                    let cond = self.emit(
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
                    let rest = acc.ok_or_else(|| {
                        "non-exhaustive scalar match has no fallthrough".to_string()
                    })?;
                    acc = Some(self.emit(
                        width,
                        Expr::Ternary {
                            c: Box::new(cond.expr),
                            t: Box::new(body.expr),
                            e: Box::new(rest.expr),
                        },
                    ));
                }
                ir::Pattern::Bool(b) => {
                    let body = self.lower_expr(&arm.body)?;
                    let cond = self.emit(
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
                    let rest = acc
                        .ok_or_else(|| "non-exhaustive bool match has no fallthrough".to_string())?;
                    acc = Some(self.emit(
                        width,
                        Expr::Ternary {
                            c: Box::new(cond.expr),
                            t: Box::new(body.expr),
                            e: Box::new(rest.expr),
                        },
                    ));
                }
                ir::Pattern::Variant { .. } => {
                    return Err("enum/variant `match` not yet supported by codegen".to_string())
                }
            }
        }
        acc.ok_or_else(|| "empty match".to_string())
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
        assert!(ty_width(&Type::Int(IntTy { signed: true, width: 32 })).is_err());
        assert_eq!(ty_width(&Type::Bit(12)).unwrap(), 12);
        assert_eq!(ty_width(&Type::Bool).unwrap(), 1);
    }
}

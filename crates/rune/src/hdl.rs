//! # HDL-subset analysis (Phase 2C)
//!
//! This module performs a **read-only classification** of every function in a
//! typed [`crate::ir::Module`], deciding whether it belongs to the
//! *synthesizable subset* — the fragment of Rune a future HDL backend could
//! lower to Verilog. **This pass emits reports only; it performs no code
//! generation and never mutates the IR.**
//!
//! ## The synthesizable subset
//!
//! A function **qualifies** iff *all* of the following hold:
//!
//! 1. **Hardware-typed signature.** Every parameter type and the return type is
//!    a *hardware type*. A hardware type is `bit<N>`, `bool`, or an array /
//!    struct / enum composed (transitively) only of hardware types. Machine
//!    integers (`i8..i64` / `u8..u64`, i.e. [`crate::ir::Type::Int`]) and
//!    `()` ([`crate::ir::Type::Unit`]) are *not* hardware types. Named-type
//!    recursion is guarded with a visited set, so cyclic struct/enum graphs are
//!    handled without looping.
//! 2. **Pure.** The body contains no `print` anywhere (no observable side
//!    effects).
//! 3. **Bounded loops only.** The body contains no `while` loop (potentially
//!    unbounded). A `for` over an integer range (bounded) is allowed.
//! 4. **Non-recursive.** The function is not part of any direct or mutual call
//!    cycle in the static call graph.
//! 5. **Synthesizable callees.** Every function it calls is itself
//!    synthesizable.
//!
//! Criteria 1–3 are *local* (decidable from a function in isolation).
//! Criteria 4–5 are *global*: recursion is detected up front via DFS on the
//! call graph, and "calls a non-synthesizable function" is resolved with a
//! fixpoint that repeatedly removes any candidate calling a rejected function
//! until the set is stable.
//!
//! All applicable failure reasons are collected for each function, so a report
//! is maximally informative rather than stopping at the first problem.

use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{Block, Expr, ExprKind, Func, Module, Stmt, Type};

/// The classification result for a single function.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionReport {
    pub name: String,
    pub synthesizable: bool,
    /// Human-readable failure reasons. Empty iff `synthesizable` is `true`.
    pub reasons: Vec<String>,
}

/// Analyze every function in `module`, returning one [`FunctionReport`] per
/// function, sorted by function name.
pub fn analyze(module: &Module) -> Vec<FunctionReport> {
    // Pass 1: collect the local (per-function) failure reasons for criteria
    // 1, 2, 3. These never depend on other functions.
    let mut local_reasons: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for func in module.funcs.values() {
        local_reasons.insert(func.name.as_str(), local_failure_reasons(func, module));
    }

    // Build the static call graph (callee names that exist as functions).
    let mut callees: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for func in module.funcs.values() {
        let mut set = BTreeSet::new();
        collect_calls_block(&func.body, &mut set);
        // Keep only edges to functions that actually exist in this module.
        set.retain(|c| module.funcs.contains_key(c));
        callees.insert(func.name.as_str(), set);
    }

    // Pass 2: recursion detection (criterion 4). Any function on a cycle —
    // including a direct self-call — is recursive.
    let recursive = find_recursive(module, &callees);

    // Pass 3: fixpoint for criteria 4 & 5. A function is synthesizable iff it
    // has no local failures, is not recursive, and (iteratively) calls only
    // synthesizable functions.
    let mut synth: BTreeSet<&str> = module
        .funcs
        .keys()
        .map(|k| k.as_str())
        .filter(|name| {
            local_reasons[name].is_empty() && !recursive.contains(*name)
        })
        .collect();

    loop {
        let mut changed = false;
        let candidates: Vec<&str> = synth.iter().copied().collect();
        for name in candidates {
            let calls_bad = callees[name]
                .iter()
                .any(|c| !synth.contains(c.as_str()));
            if calls_bad {
                synth.remove(name);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Pass 4: assemble reports. Reasons combine local failures, recursion, and
    // each non-synthesizable callee.
    let mut reports = Vec::with_capacity(module.funcs.len());
    for func in module.funcs.values() {
        let name = func.name.as_str();
        let synthesizable = synth.contains(name);
        let mut reasons = local_reasons[name].clone();

        if recursive.contains(name) {
            reasons.push("is recursive".to_string());
        }

        // Report each callee that is not synthesizable (stable, sorted order).
        for callee in &callees[name] {
            if !synth.contains(callee.as_str()) {
                reasons.push(format!(
                    "calls non-synthesizable function `{}`",
                    callee
                ));
            }
        }

        // Defensive consistency: a synthesizable function has no reasons, and a
        // non-synthesizable one always has at least one.
        if synthesizable {
            reasons.clear();
        }

        reports.push(FunctionReport {
            name: func.name.clone(),
            synthesizable,
            reasons,
        });
    }

    reports.sort_by(|a, b| a.name.cmp(&b.name));
    reports
}

/// Render a multi-line human-readable summary of the reports.
pub fn report_string(reports: &[FunctionReport]) -> String {
    let mut out = String::new();
    for (i, r) in reports.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if r.synthesizable {
            out.push_str(&format!("\u{2713} {} \u{2014} synthesizable", r.name));
        } else {
            out.push_str(&format!(
                "\u{2717} {} \u{2014} not synthesizable: {}",
                r.name,
                r.reasons.join("; ")
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Local (per-function) checks: criteria 1, 2, 3.
// ---------------------------------------------------------------------------

/// Collect the criterion-1/2/3 failure reasons for `func` in isolation.
fn local_failure_reasons(func: &Func, module: &Module) -> Vec<String> {
    let mut reasons = Vec::new();

    // Criterion 1: hardware-typed signature.
    for param in &func.params {
        if !is_hardware_type(&param.ty, module) {
            reasons.push(format!(
                "parameter `{}` has type `{}`, which contains non-hardware (machine-integer) types",
                param.name, param.ty
            ));
        }
    }
    if !is_hardware_type(&func.ret, module) {
        reasons.push(format!(
            "returns non-hardware type `{}`",
            func.ret
        ));
    }

    // Criterion 2: pure (no print). Criterion 3: no while loop. Both are found
    // with a single structural walk over the body.
    let mut uses_print = false;
    let mut uses_while = false;
    scan_block(&func.body, &mut uses_print, &mut uses_while);
    if uses_print {
        reasons.push("uses `print` (not a pure function)".to_string());
    }
    if uses_while {
        reasons.push("contains a `while` loop (unbounded)".to_string());
    }

    reasons
}

/// True iff `ty` is a *hardware type*: `bit<N>`, `bool`, or an array / struct /
/// enum composed transitively of hardware types. `visited` guards against
/// cyclic named-type graphs.
fn is_hardware_type(ty: &Type, module: &Module) -> bool {
    fn go(ty: &Type, module: &Module, visited: &mut BTreeSet<String>) -> bool {
        match ty {
            Type::Bool | Type::Bit(_) => true,
            Type::Int(_) | Type::Unit => false,
            Type::Array(elem, _) => go(elem, module, visited),
            Type::Struct(name) => {
                if !visited.insert(name.clone()) {
                    // Already on the path: cycles don't introduce new
                    // non-hardware types, so treat as satisfied here.
                    return true;
                }
                let ok = match module.structs.get(name) {
                    Some(def) => def.fields.iter().all(|f| go(&f.ty, module, visited)),
                    // Unknown named type: cannot prove it is hardware.
                    None => false,
                };
                visited.remove(name);
                ok
            }
            Type::Enum(name) => {
                if !visited.insert(name.clone()) {
                    return true;
                }
                let ok = match module.enums.get(name) {
                    Some(def) => def
                        .variants
                        .iter()
                        .all(|v| v.fields.iter().all(|t| go(t, module, visited))),
                    None => false,
                };
                visited.remove(name);
                ok
            }
            Type::Tuple(ts) => ts.iter().all(|t| go(t, module, visited)),
        }
    }
    go(ty, module, &mut BTreeSet::new())
}

/// Walk a block, setting `uses_print` / `uses_while` if either is found
/// anywhere (including nested blocks, expressions, match arms, etc.).
fn scan_block(block: &Block, uses_print: &mut bool, uses_while: &mut bool) {
    for stmt in &block.stmts {
        scan_stmt(stmt, uses_print, uses_while);
    }
    if let Some(tail) = &block.tail {
        scan_expr(tail, uses_print, uses_while);
    }
}

fn scan_stmt(stmt: &Stmt, uses_print: &mut bool, uses_while: &mut bool) {
    match stmt {
        Stmt::Let { init, .. } => scan_expr(init, uses_print, uses_while),
        Stmt::Assign { place, value } => {
            scan_place(place, uses_print, uses_while);
            scan_expr(value, uses_print, uses_while);
        }
        Stmt::While { cond, body } => {
            *uses_while = true;
            scan_expr(cond, uses_print, uses_while);
            scan_block(body, uses_print, uses_while);
        }
        Stmt::For { lo, hi, body, .. } => {
            scan_expr(lo, uses_print, uses_while);
            scan_expr(hi, uses_print, uses_while);
            scan_block(body, uses_print, uses_while);
        }
        Stmt::Return { value } => {
            if let Some(v) = value {
                scan_expr(v, uses_print, uses_while);
            }
        }
        Stmt::Expr(e) => scan_expr(e, uses_print, uses_while),
    }
}

fn scan_place(place: &crate::ir::Place, uses_print: &mut bool, uses_while: &mut bool) {
    use crate::ir::Place;
    match place {
        Place::Local { .. } => {}
        Place::Field { base, .. } | Place::TupleField { base, .. } => {
            scan_place(base, uses_print, uses_while)
        }
        Place::Index { base, index, .. } => {
            scan_place(base, uses_print, uses_while);
            scan_expr(index, uses_print, uses_while);
        }
    }
}

fn scan_expr(expr: &Expr, uses_print: &mut bool, uses_while: &mut bool) {
    match &expr.kind {
        ExprKind::Int(_) | ExprKind::Bool(_) | ExprKind::Unit | ExprKind::Local(_) => {}
        ExprKind::Unary { expr, .. } => scan_expr(expr, uses_print, uses_while),
        ExprKind::Binary { lhs, rhs, .. } => {
            scan_expr(lhs, uses_print, uses_while);
            scan_expr(rhs, uses_print, uses_while);
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                scan_expr(a, uses_print, uses_while);
            }
        }
        ExprKind::Print { arg } => {
            *uses_print = true;
            scan_expr(arg, uses_print, uses_while);
        }
        ExprKind::MakeStruct { fields, .. } => {
            for f in fields {
                scan_expr(f, uses_print, uses_while);
            }
        }
        ExprKind::MakeEnum { args, .. } => {
            for a in args {
                scan_expr(a, uses_print, uses_while);
            }
        }
        ExprKind::MakeTuple(elems) => {
            for e in elems {
                scan_expr(e, uses_print, uses_while);
            }
        }
        ExprKind::Field { base, .. } | ExprKind::TupleField { base, .. } => {
            scan_expr(base, uses_print, uses_while)
        }
        ExprKind::Index { base, index } => {
            scan_expr(base, uses_print, uses_while);
            scan_expr(index, uses_print, uses_while);
        }
        ExprKind::Array(elems) => {
            for e in elems {
                scan_expr(e, uses_print, uses_while);
            }
        }
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            scan_expr(cond, uses_print, uses_while);
            scan_block(then_branch, uses_print, uses_while);
            scan_block(else_branch, uses_print, uses_while);
        }
        ExprKind::Match { scrutinee, arms } => {
            scan_expr(scrutinee, uses_print, uses_while);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    scan_expr(g, uses_print, uses_while);
                }
                scan_expr(&arm.body, uses_print, uses_while);
            }
        }
        ExprKind::Block(b) => scan_block(b, uses_print, uses_while),
    }
}

// ---------------------------------------------------------------------------
// Call graph: collect direct callees by name.
// ---------------------------------------------------------------------------

fn collect_calls_block(block: &Block, out: &mut BTreeSet<String>) {
    for stmt in &block.stmts {
        collect_calls_stmt(stmt, out);
    }
    if let Some(tail) = &block.tail {
        collect_calls_expr(tail, out);
    }
}

fn collect_calls_stmt(stmt: &Stmt, out: &mut BTreeSet<String>) {
    match stmt {
        Stmt::Let { init, .. } => collect_calls_expr(init, out),
        Stmt::Assign { place, value } => {
            collect_calls_place(place, out);
            collect_calls_expr(value, out);
        }
        Stmt::While { cond, body } => {
            collect_calls_expr(cond, out);
            collect_calls_block(body, out);
        }
        Stmt::For { lo, hi, body, .. } => {
            collect_calls_expr(lo, out);
            collect_calls_expr(hi, out);
            collect_calls_block(body, out);
        }
        Stmt::Return { value } => {
            if let Some(v) = value {
                collect_calls_expr(v, out);
            }
        }
        Stmt::Expr(e) => collect_calls_expr(e, out),
    }
}

fn collect_calls_place(place: &crate::ir::Place, out: &mut BTreeSet<String>) {
    use crate::ir::Place;
    match place {
        Place::Local { .. } => {}
        Place::Field { base, .. } | Place::TupleField { base, .. } => {
            collect_calls_place(base, out)
        }
        Place::Index { base, index, .. } => {
            collect_calls_place(base, out);
            collect_calls_expr(index, out);
        }
    }
}

fn collect_calls_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match &expr.kind {
        ExprKind::Int(_) | ExprKind::Bool(_) | ExprKind::Unit | ExprKind::Local(_) => {}
        ExprKind::Unary { expr, .. } => collect_calls_expr(expr, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_calls_expr(lhs, out);
            collect_calls_expr(rhs, out);
        }
        ExprKind::Call { func, args } => {
            out.insert(func.clone());
            for a in args {
                collect_calls_expr(a, out);
            }
        }
        ExprKind::Print { arg } => collect_calls_expr(arg, out),
        ExprKind::MakeStruct { fields, .. } => {
            for f in fields {
                collect_calls_expr(f, out);
            }
        }
        ExprKind::MakeEnum { args, .. } => {
            for a in args {
                collect_calls_expr(a, out);
            }
        }
        ExprKind::MakeTuple(elems) => {
            for e in elems {
                collect_calls_expr(e, out);
            }
        }
        ExprKind::Field { base, .. } | ExprKind::TupleField { base, .. } => {
            collect_calls_expr(base, out)
        }
        ExprKind::Index { base, index } => {
            collect_calls_expr(base, out);
            collect_calls_expr(index, out);
        }
        ExprKind::Array(elems) => {
            for e in elems {
                collect_calls_expr(e, out);
            }
        }
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_calls_expr(cond, out);
            collect_calls_block(then_branch, out);
            collect_calls_block(else_branch, out);
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_calls_expr(scrutinee, out);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    collect_calls_expr(g, out);
                }
                collect_calls_expr(&arm.body, out);
            }
        }
        ExprKind::Block(b) => collect_calls_block(b, out),
    }
}

// ---------------------------------------------------------------------------
// Recursion detection: any function on a cycle (direct or mutual) in the call
// graph. Implemented with iterative DFS plus an "on-stack" set.
// ---------------------------------------------------------------------------

fn find_recursive<'a>(
    module: &'a Module,
    callees: &BTreeMap<&'a str, BTreeSet<String>>,
) -> BTreeSet<&'a str> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Unvisited,
        OnStack,
        Done,
    }

    let mut state: BTreeMap<&str, State> = module
        .funcs
        .keys()
        .map(|k| (k.as_str(), State::Unvisited))
        .collect();
    let mut recursive: BTreeSet<&str> = BTreeSet::new();

    for start in module.funcs.keys().map(|k| k.as_str()) {
        if state[start] != State::Unvisited {
            continue;
        }
        // Iterative DFS. Each frame tracks the node and an iterator over its
        // remaining children.
        let mut stack: Vec<(&str, Vec<&str>, usize)> = Vec::new();
        let children: Vec<&str> = callees[start]
            .iter()
            .filter_map(|c| module.funcs.get_key_value(c).map(|(k, _)| k.as_str()))
            .collect();
        *state.get_mut(start).unwrap() = State::OnStack;
        stack.push((start, children, 0));

        while let Some(&(node, ref kids, idx)) = stack.last() {
            if idx < kids.len() {
                let child = kids[idx];
                // Advance the parent's cursor.
                stack.last_mut().unwrap().2 += 1;

                match state[child] {
                    State::OnStack => {
                        // Back-edge: every node currently on the stack from
                        // `child` upward is part of this cycle. Simpler and
                        // sufficient: mark both endpoints; the fixpoint and
                        // full-stack marking below handle mutual recursion.
                        // Mark all nodes currently on the stack at or above
                        // `child` as recursive.
                        let mut in_cycle = false;
                        for (n, _, _) in &stack {
                            if *n == child {
                                in_cycle = true;
                            }
                            if in_cycle {
                                recursive.insert(*n);
                            }
                        }
                        recursive.insert(child);
                    }
                    State::Unvisited => {
                        let gkids: Vec<&str> = callees[child]
                            .iter()
                            .filter_map(|c| {
                                module.funcs.get_key_value(c).map(|(k, _)| k.as_str())
                            })
                            .collect();
                        *state.get_mut(child).unwrap() = State::OnStack;
                        stack.push((child, gkids, 0));
                    }
                    State::Done => {}
                }
                let _ = node;
            } else {
                // Exhausted children: pop and mark done.
                *state.get_mut(node).unwrap() = State::Done;
                stack.pop();
            }
        }
    }

    recursive
}

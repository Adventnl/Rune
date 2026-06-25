# The Rune Typed Core IR — Design Note

This document records the invariants of the typed core IR (`crates/rune/src/ir.rs`).
The IR is the **stable contract** consumed by the interpreter today and by a
future HDL backend. It is *frozen*: Phase-2 work consumes it, never edits it. If
the IR is found genuinely insufficient, stop, fix it once centrally, then resume.

## Why a separate typed IR?

The AST (`ast.rs`) mirrors surface syntax: names are unresolved strings, integer
literals have no committed width, and `match` exhaustiveness is unchecked. The
IR is what you get *after* typechecking: every expression carries a resolved
`Type`, every name is resolved, every width is fixed, and every `match` is
proven exhaustive. Downstream stages never re-derive types or re-resolve names.

```
source → lexer → parser → AST → [typeck] → TYPED CORE IR → interpreter
```

## Invariants

1. **Fully typed.** `ir::Expr` is `{ kind: ExprKind, ty: Type }`. The
   typechecker is the only producer of IR.

2. **Deterministic, no undefined behavior.**
   - Integer overflow is **wrapping** (two's complement) for *every*
     integer-like type — `i8..i64`, `u8..u64`, and `bit<N>`. The interpreter and
     any future hardware target therefore agree bit-for-bit.
   - Division / remainder by zero is a **defined runtime trap** (a reported
     `Diagnostic`), never UB.
   - Shift amounts are reduced modulo the operand width, so shifts are total.
   - Array indexing is bounds-checked at runtime (defined trap on out-of-range).

3. **Explicit bit widths — the HDL bridge.** `Type::Bit(N)` carries its width.
   Arithmetic on `bit<N>` wraps modulo `2^N`. These semantics are exact so the
   synthesizable subset maps cleanly to hardware. `1 <= N <= 128`.

4. **Value semantics, no heap.** `bool`, integers, `bit<N>`, arrays, structs,
   and enums are all values. No references, no aliasing, no allocation, no GC in
   the core. Assignment and parameter passing copy.

5. **Small, explicit, SSA-friendly.** Control flow is structured
   (`if`/`while`/`for`/`match`). Enum variants are referenced by numeric `tag`;
   struct fields by numeric `index`. Bounded `for` loops range over a half-open
   integer interval. There is no recursion-free guarantee at the IR level (the
   interpreter allows recursion); the **HDL-subset pass** is what flags
   recursion, unbounded loops, and other non-synthesizable constructs.

## Type system summary

| Surface          | IR `Type`                |
|------------------|--------------------------|
| `bool`           | `Bool`                   |
| `i8`..`i64`      | `Int { signed: true, .. }`  |
| `u8`..`u64`      | `Int { signed: false, .. }` |
| `bit<N>`         | `Bit(N)`                 |
| `[T; N]`         | `Array(T, N)`            |
| `struct S`       | `Struct("S")`            |
| `enum E`         | `Enum("E")`              |
| `()`             | `Unit`                   |

Integer literals are checked to fit their inferred target type. There are **no
implicit conversions**: mixing `u8` and `u32`, or `bit<8>` and `u8`, in one
binary operation is a type error. Comparisons require matching operand types and
yield `bool`.

## The synthesizable subset (marked, not enforced)

A function is in the synthesizable subset when it is: pure (no `print`, no
side effects), operates only over `bit<N>`/`bool`/arrays/structs/enums of those,
uses only bounded `for` loops (no `while`), and is non-recursive. The
`hdl` analysis pass reports which functions qualify and why each non-qualifier
fails. **No HDL codegen is produced in this project.**

## Hot reload contract

The IR keeps definitions in name-keyed maps (`Module::funcs`, `structs`,
`enums`) so a single definition can be re-typechecked and swapped without
disturbing the rest. `Func::signature()` exposes the `(params, ret)` signature;
hot reload uses signature equality to decide whether live state stays valid or a
breaking change must be reported (never crashed).

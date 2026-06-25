# Rune

Rune is a small, statically-typed systems language (C++ in spirit: value
semantics, explicit types, no GC, low-level control), architected around **one
typed core IR** with two intended targets:

1. a **live, hot-reloadable interpreter** — built here, and
2. a **synthesizable subset** intended to lower to Verilog/HDL later — this
   project only *analyses* and marks that subset; it does **not** generate
   hardware.

The interpreter runs the full language. Only a restricted, statically-analyzable
subset is ever meant to reach hardware, and the core IR is kept HDL-friendly
(deterministic, no UB, explicit bit widths) so that backend stays reachable.

## Pipeline

```
source → lexer → parser → AST → TYPED CORE IR → interpreter over IR
```

The typed core IR (`crates/rune/src/ir.rs`, design note in `docs/ir.md`) is the
stable contract shared by every stage.

## The language (v1)

- **Types:** `bool`; fixed-width integers `i8 i16 i32 i64` / `u8 u16 u32 u64`;
  `bit<N>` (explicit bit-vector with **wrapping** overflow — the HDL bridge);
  fixed-size arrays `[T; N]`; `struct`; tagged `enum` (sum types).
- **Values & semantics:** value semantics, explicit mutability via `let` /
  `let mut`, no heap, no GC, fully deterministic, **no undefined behavior**.
  Arithmetic wraps (two's complement) so the interpreter and any future hardware
  agree bit-for-bit. Divide-by-zero and out-of-bounds indexing are defined
  runtime traps.
- **Code:** functions; exhaustive `match`; `if`/`while`; `for` over a bounded
  integer range. Integer literals infer their width from context (default
  `i32`); there are no implicit conversions.

### Example (the milestone program)

```rune
fn add8(a: bit<8>, b: bit<8>) -> bit<8> {
    a + b            // wrapping: add8(200, 100) == 44
}

enum Shape { Circle(u32), Rect(u32, u32) }

fn area(s: Shape) -> u32 {
    match s {
        Circle(r)   => 3 * r * r,
        Rect(w, h)  => w * h,
    }
}

fn main() {
    print(add8(200, 100));   // 44
    print(area(Rect(3, 4))); // 12
    print(area(Circle(2)));  // 12
}
```

## Building & running

```sh
cargo build --workspace
cargo test  --workspace          # full suite

cargo run -p runec -- run   examples/milestone.rune   # run a program
cargo run -p runec -- repl                            # interactive REPL
cargo run -p runec -- watch examples/milestone.rune   # hot-reload on edits
cargo run -p runec -- hdl   examples/milestone.rune   # synthesizability report
```

### REPL

The REPL evaluates expressions, definitions (`fn`/`struct`/`enum`), and
persistent `let` statements; redefining a name replaces it. Commands:
`:help`, `:list`, `:reset`, `:load <file>`, `:quit`.

### Hot reload

Definitions (functions/types) are separated from live runtime state. On reload,
changed definitions are re-typechecked and swapped in; compatible live state is
preserved and incompatibilities (signature changes, struct/enum shape changes,
compile errors) are **reported, not crashed**. See
`crates/rune/src/hotreload.rs`.

### HDL-subset analysis (reports only)

`runec hdl <file>` classifies each function as synthesizable or not, with
reasons. A function qualifies when it is pure (no `print`), has a hardware-typed
signature (`bit<N>`/`bool` and composites thereof — not machine integers), uses
only bounded loops (no `while`), is non-recursive, and calls only synthesizable
functions. **No HDL is generated.**

## Workspace layout

| Crate / module                     | Responsibility                          |
|------------------------------------|-----------------------------------------|
| `crates/rune/src/lexer.rs`         | tokenizer                               |
| `crates/rune/src/parser.rs`        | recursive-descent + Pratt parser → AST  |
| `crates/rune/src/ast.rs`           | surface syntax tree                     |
| `crates/rune/src/typeck.rs`        | AST → typed core IR, full checking      |
| `crates/rune/src/ir.rs`            | **frozen** typed core IR (the contract) |
| `crates/rune/src/interp.rs`        | tree-walking evaluator over the IR      |
| `crates/rune/src/hotreload.rs`     | definition registry, reload, file watch |
| `crates/rune/src/hdl.rs`           | synthesizable-subset analysis (reports) |
| `crates/runec/`                    | CLI: `run` / `repl` / `watch` / `hdl`   |

## Non-goals (v1)

HDL/Verilog codegen, an LLVM/native backend, optimization passes, generics, a
module system beyond namespacing, a standard library beyond primitives, FFI, and
a package manager are all out of scope for this phase.

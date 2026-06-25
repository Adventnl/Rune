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
- **Namespacing:** `mod` modules (inline or file-based), `use` imports, and
  `::` paths. See *Modules, the standard library, and packages* below.

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

cargo run -p runec -- run   examples/milestone.rune   # run a single file
cargo run -p runec -- run   examples/modules.rune     # modules + stdlib
cargo run -p runec -- repl                            # interactive REPL
cargo run -p runec -- watch examples/milestone.rune   # hot-reload on edits
cargo run -p runec -- hdl   examples/milestone.rune   # synthesizability report

# Packages
cargo run -p runec -- new   mypkg                     # scaffold a package
cargo run -p runec -- build examples/pkg/demo         # resolve + typecheck
cargo run -p runec -- run   examples/pkg/demo         # build + run main()
cargo run -p runec -- test  examples/pkg/demo         # run test_* functions
```

## Modules, the standard library, and packages

**Modules** namespace definitions. Use `mod m { ... }` inline, or `mod m;` to
pull in a sibling `m.rune` file. Refer to items by path and shorten with `use`:

```rune
mod geom {
    enum Shape { Circle(u32), Rect(u32, u32) }
    fn area(s: Shape) -> u32 {
        match s { Circle(r) => 3 * r * r, Rect(w, h) => w * h }
    }
}

use std::math::clamp_u32;

fn main() {
    print(geom::area(geom::Shape::Rect(3, 4)));  // 12  (enum-qualified variant)
    print(clamp_u32(250, 0, 64));                // 64  (via `use`)
}
```

Items are keyed internally by fully-qualified name (`geom::area`); root-level
programs are unaffected. Enum variants are enum-qualified (`Shape::Rect`), or
bare when the enum is in scope. Variant names need only be unique per enum.

**Standard library** (`std/*.rune`, written in Rune): `std::math`
(`min_u32`/`max_u32`/`clamp_u32`/`gcd_u32`/`pow_u32`/`abs_i32`/`sum_to_u32`) and
`std::bits` (`popcount32`/`parity32`/`reverse32`/`rotl32`/`rotr32`/`get_bit32`
on `bit<32>`). There are no generics in v1, so these are monomorphic. The driver
injects `std` automatically; override its location with the `RUNE_STD` env var.

**Packages** are directories with a `rune.toml`:

```toml
[package]
name = "demo"
version = "0.1.0"
entry = "src/main.rune"        # optional (this is the default)

[dependencies]
mathx = { path = "../mathx" }  # local path dependency, usable as `mathx::...`
```

`runec build` resolves the entry file, its file modules, `std`, and path
dependencies, then typechecks (no codegen). `runec test` runs every zero-arg
`fn test_*() -> bool`, treating `false` or a trap as failure. See
`examples/pkg/demo` (which depends on `examples/pkg/mathx`).

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
| `crates/rune/src/typeck.rs`        | AST → typed core IR, name resolution    |
| `crates/rune/src/ir.rs`            | **frozen** typed core IR (the contract) |
| `crates/rune/src/interp.rs`        | tree-walking evaluator over the IR      |
| `crates/rune/src/loader.rs`        | file modules + `std` injection          |
| `crates/rune/src/hotreload.rs`     | definition registry, reload, file watch |
| `crates/rune/src/hdl.rs`           | synthesizable-subset analysis (reports) |
| `crates/runec/`                    | CLI: run/repl/watch/hdl/new/build/test  |
| `std/`                             | standard library, written in Rune       |

## Status & non-goals

Built so far: the typed-IR front end and interpreter (Phases 0–2), hot reload,
the HDL-subset analysis, and — most recently — a module system, a Rune-written
standard library, and package tooling (`rune.toml`, `new`/`build`/`test`, path
dependencies).

Still out of scope: HDL/Verilog codegen, an LLVM/native backend, optimization
passes, generics, FFI, and remote/registry package dependencies (only local
`path` deps are supported). The synthesizable subset is *analysed*, never
lowered to hardware here.

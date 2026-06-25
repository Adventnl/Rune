# The Rune Verilog Backend — Design Note

This records the invariants of the Verilog backend (`crates/rune/src/verilog.rs`),
which lowers the **synthesizable subset** of the typed core IR to synthesizable
Verilog. It consumes the frozen IR (`ir.rs`) and the analysis pass (`hdl.rs`);
it does not modify them.

## The equivalence contract

The Rune interpreter is the executable specification. Generated hardware must
compute the **same function, bit-for-bit**, for every input. Two facts make this
non-trivial and drive the whole design:

1. **Rune wraps per operation.** `bit<N>` arithmetic is modulo `2^N` at *each*
   step: in `(a + b) * c`, the `a + b` is already truncated to `N` bits before
   the multiply.
2. **Verilog wraps per assignment.** A Verilog expression is evaluated at one
   context-determined width and truncated only when assigned to a sized net. A
   naive transcription of `(a + b) * c` would keep `a + b` at full precision.

**Resolution — SSA lowering.** Every IR operation becomes its own sized `wire`.
Assigning each intermediate to a wire of the operation's exact width forces
truncation at every step, so the netlist wraps exactly where the interpreter
does. This is why the emitted Verilog is a flat list of single-operation wires
rather than nested expressions.

## Gating

Codegen is gated by `hdl::analyze`. A function is lowered only if that pass
marks it synthesizable; otherwise it is recorded in `LowerResult::skipped` with
the pass's reasons and emitted as a `// skipped` comment — **never mis-lowered**.
A second gate (`codegen unsupported: …`) covers synthesizable functions that use
a construct this phase doesn't lower yet.

## Encoding

| Rune type | Verilog |
|-----------|---------|
| `bool`    | 1 bit |
| `bit<N>`  | `[N-1:0]` |
| machine ints `iN`/`uN` | **not lowered** (not hardware types) |
| `struct` / `enum` / array | packed bit-vectors (later phase) |

- **Wrapping:** results assigned to sized wires; multiply truncates `2N→N`.
- **Shifts:** the shift amount is reduced modulo the width — a mask `& (N-1)` for
  power-of-two `N`, else `% N` — mirroring the interpreter's total shifts.
- **Comparisons / logicals:** yield a 1-bit wire. Operands are unsigned, so
  Verilog's unsigned relational operators match `bit<N>` ordering.

## Module convention

Each function → one module: an `input` port per parameter (named after the
parameter) and a single `output` port named `out`. Calls become module
instances connected by parameter name, with the callee's `out` wired to a fresh
net. Fully-qualified names are sanitized (`std::bits::rotl32` →
`std__bits__rotl32`). A parameter literally named `out` is rejected.

## Verification without a simulator

When no HDL simulator (`iverilog`/`verilator`) is available, equivalence is
proven by a **reference evaluator** (`verilog::eval`) that executes the emitted
netlist with Verilog semantics (unsigned, each wire masked to its width) and is
checked against the interpreter over many inputs (exhaustive for small widths,
randomized for wide ones). Golden snapshots lock the emitted text. Where a
simulator *is* present, a generated self-checking testbench enables true
cosimulation. The interpreter is always the oracle.

## Scope

Lowered now: scalar hardware types (`bit<N>`/`bool`), all operators, immutable
`let`, `if`/`match` (scalar patterns) as value expressions, and calls. Deferred:
`struct`/`enum`/array packing and enum `match`, bounded-`for` unrolling, and
mutable-local/assignment SSA. Out of scope entirely: clocked/sequential logic,
optimization, and lowering anything the analysis pass rejects.

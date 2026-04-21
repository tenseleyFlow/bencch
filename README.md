# bencch

Structured compiler runner and reporting bench for `armfortas`.

This repo holds:

- `bench-core/` — bench-owned compiler-facing types
- `bench/` — the `afs-tests` runner
- `suites/` — authored bench suites
- `fixtures/` — reusable fixture programs
- `reports/` — failure and consistency bundles

## Current Setup

`bencch` is no longer aiming to be the primary product story for a generic
compiler bench. Its role is clearer now:

- the root armfortas harness is the fast armfortas-first runner and the default
  home for new source-directed testing ideas
- `bencch` is the structured matrix/reporting/differential runner around that
  same testing language

Source comments in shared fixtures are the canonical leaf-assertion language.
`bencch` should consume those directives where supported and explain
unsupported directives clearly, rather than inventing a separate assertion
dialect.

Today `bencch` is wired to a surrounding `armfortas` checkout. The practical way
to use it is from the `armfortas` workspace root. CLI-side compiler and tool
paths are overridable now; linked capture still comes from the surrounding
workspace. That linked compiler surface is currently isolated in
`bench/src/compiler.rs`, and the bench-owned compiler-facing types now live in
`bench-core/`.

```bash
cargo run -p afs-tests -- list
cargo run -p afs-tests -- run --suite frontend
```

Standalone compiler adapters are not finished yet.

## Usage

List suites:

```bash
cargo run -p afs-tests -- list
```

Run one suite family:

```bash
cargo run -p afs-tests -- run --suite consistency/runtime
```

Run against an explicit compiler binary:

```bash
cargo run -p afs-tests -- run --suite consistency/runtime-control-flow --armfortas-bin ./target/debug/armfortas
```

Run differential checks with explicit reference compiler paths:

```bash
cargo run -p afs-tests -- run --suite differential/runtime-control-flow --gfortran-bin /opt/homebrew/bin/gfortran --flang-bin /opt/homebrew/bin/flang-new
```

List the staged real-project differential ladder:

```bash
cargo run -p afs-tests -- projects list
```

Run one real project through its native build system with `armfortas` and `flang-new`:

```bash
cargo run -p afs-tests -- projects run --project fortbite --armfortas-bin ./target/release/armfortas
```

Run one case with full stage capture:

```bash
cargo run -p afs-tests -- run --suite frontend --case stage_walk --all --verbose
```

Run consistency coverage:

```bash
cargo run -p afs-tests -- run --suite consistency --all
```

Run differential coverage:

```bash
cargo run -p afs-tests -- run --suite differential
```

Reports are written under `bencch/reports/`.

Project differential reports land under `bencch/reports/projects/`.

Environment overrides work too:

```bash
BENCCH_ARMFORTAS_BIN=./target/debug/armfortas cargo run -p afs-tests -- run --suite consistency/object
```

## Suite Format

Suites are plain text files under `suites/`.

```text
suite "consistency/runtime"

case "mixed_types_cli_run_reproducible"
source "../../fixtures/runtime/mixed_types.f90"
opts => all
armfortas => run
repeat => 3
consistency => cli_run_reproducible
expect run.stdout check-comments
expect run.exit_code equals 0
end
```

Graph cases use `entry` plus ordered `file` lines:

```text
suite "modules/runtime-graphs"

case "module_chain_runtime"
entry "../../fixtures/modules/module_chain/main.f90"
file "../../fixtures/modules/module_chain/math_seed.f90"
file "../../fixtures/modules/module_chain/math_values.f90"
file "../../fixtures/modules/module_chain/main.f90"
opts => O0, O1, O2
armfortas => run
expect run.stdout check-comments
end
```

Today the armfortas adapter materializes graph cases into one generated source
in declared file order before capture/compile. The authored files still stay in
the failure bundle.

Common things the runner understands:

- stage capture like `armfortas => tokens, ir, asm, obj, run`
- opt matrices like `opts => O0, O1, O2`
- references like `differential => gfortran, flang-new`
- expected failures like `xfail "reason"`
- per-opt status like `xfail when O1, O2 because "reason"`
- consistency checks like `cli_obj_vs_system_as` and `capture_run_reproducible`

The suite DSL is for orchestration:

- opts
- compilers and references
- graph composition
- capability policy
- reporting and bundles

Leaf assertions should come from shared source directives whenever possible.

## Notes

- `.docs/` is local and gitignored.
- The runner is currently strongest on matrices, differential behavior,
  capability-aware execution, reports, bundles, and graph orchestration.
- The shared-language reset and follow-through testing roadmap live in the
  parent `armfortas` repo under `.docs/testing/`.

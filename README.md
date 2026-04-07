# bencch

Compiler bench for `armfortas`.

This repo holds:

- `bench-core/` — bench-owned compiler-facing types
- `bench/` — the `afs-tests` runner
- `suites/` — authored bench suites
- `fixtures/` — reusable fixture programs
- `reports/` — failure and consistency bundles

## Current Setup

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

Environment overrides work too:

```bash
BENCCH_ARMFORTAS_BIN=./target/debug/armfortas cargo run -p afs-tests -- run --suite consistency/object
```

## Suite Format

Suites are plain text files under `suites/`.

```text
suite "consistency/runtime"

case "mixed_types_cli_run_reproducible"
source "../../../test_programs/mixed_types.f90"
opts => all
armfortas => run
repeat => 3
consistency => cli_run_reproducible
expect run.stdout check-comments
expect run.exit_code equals 0
end
```

Common things the runner understands:

- stage capture like `armfortas => tokens, ir, asm, obj, run`
- opt matrices like `opts => O0, O1, O2`
- references like `differential => gfortran, flang-new`
- expected failures like `xfail "reason"`
- per-opt status like `xfail when O1, O2 because "reason"`
- consistency checks like `cli_obj_vs_system_as` and `capture_run_reproducible`

## Notes

- `.docs/` is local and gitignored.
- The runner is currently strongest on stage capture, differential behavior,
  and consistency work around reproducibility and cross-path mismatches.

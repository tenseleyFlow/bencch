# bencch

Compiler bench for `armfortas`.

This repo holds:

- `bench/` — the `afs-tests` runner
- `suites/` — authored bench suites
- `fixtures/` — reusable fixture programs
- `reports/` — failure and consistency bundles

## Current Setup

Today `bencch` is wired to a surrounding `armfortas` checkout. The practical way
to use it is from the `armfortas` workspace root.

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

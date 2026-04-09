# bencch

Generic compiler bench, with `armfortas` as the first rich adapter.

This repo holds:

- `bench-core/` — bench-owned compiler-facing types
- `bench/` — the `bencch` / `afs-tests` runner
- `suites/` — authored bench suites
- `fixtures/` — reusable fixture programs
- `reports/` — failure and consistency bundles

## Current Setup

`bencch` now has its own workspace manifest and public CLI.

Today it is still wired to a surrounding `armfortas` checkout for linked
capture. CLI-side compiler and tool paths are overridable now; linked capture
still comes from the surrounding workspace. That linked compiler surface is
currently isolated in `bench/src/compiler.rs`, and the bench-owned
compiler-facing types now live in `bench-core/`. `bencch doctor` reports
named-adapter resolution, generic external-driver posture, and the linked
capture boundary. CLI-observable cases using `asm`, `obj`, and `run` can
already use an external `armfortas` binary as the primary execution path;
richer stage capture is still linked.

```bash
cargo run -p afs-tests --bin bencch -- list
cargo run -p afs-tests --bin bencch -- run --suite frontend
```

Standalone compiler adapters are not finished yet.

## Usage

List suites:

```bash
cargo run -p afs-tests --bin bencch -- list
```

Run one suite family:

```bash
cargo run -p afs-tests --bin bencch -- run --suite consistency/runtime
```

Inspect the current embedded/standalone posture:

```bash
cargo run -p afs-tests --bin bencch -- doctor
```

Compare two compilers on one program:

```bash
cargo run -p afs-tests --bin bencch -- compare armfortas gfortran --program fixtures/runtime/mixed_types.f90
```

Compare named compilers with an explicit armfortas binary:

```bash
cargo run -p afs-tests --bin bencch -- compare armfortas gfortran --program fixtures/runtime/if_else.f90 --armfortas-bin ../target/debug/armfortas
```

The same compare surface works across opt levels too:

```bash
cargo run -p afs-tests --bin bencch -- compare armfortas gfortran --opt O2 --program fixtures/runtime/mixed_types.f90 --armfortas-bin ../target/debug/armfortas
```

Compare with an extra artifact diff:

```bash
cargo run -p afs-tests --bin bencch -- compare armfortas gfortran --program fixtures/runtime/mixed_types.f90 --artifact asm
```

Compare two explicit compiler binaries:

```bash
cargo run -p afs-tests --bin bencch -- compare /path/to/one /path/to/other --program fixtures/runtime/mixed_types.f90 --artifact asm,obj
```

Introspect one compiler on one program:

```bash
cargo run -p afs-tests --bin bencch -- introspect armfortas fixtures/runtime/mixed_types.f90
```

Introspect a rich armfortas stage explicitly:

```bash
cargo run -p afs-tests --bin bencch -- introspect armfortas fixtures/runtime/mixed_types.f90 --artifact armfortas.ir,asm
```

Introspect the full linked armfortas stage surface:

```bash
cargo run -p afs-tests --bin bencch -- introspect armfortas fixtures/runtime/mixed_types.f90 --all
```

Introspect a named external compiler on the generic surface:

```bash
cargo run -p afs-tests --bin bencch -- introspect gfortran fixtures/runtime/if_else.f90 --artifact asm,obj,runtime
```

Run against an explicit compiler binary:

```bash
cargo run -p afs-tests --bin bencch -- run --suite consistency/runtime-control-flow --armfortas-bin ./target/debug/armfortas
```

Run an asm/object surface through an explicit compiler binary:

```bash
cargo run -p afs-tests --bin bencch -- run --suite backend/asm --case runtime_wrapper_and_calls --armfortas-bin ./target/debug/armfortas
```

Run differential checks with explicit reference compiler paths:

```bash
cargo run -p afs-tests --bin bencch -- run --suite differential/runtime-control-flow --gfortran-bin /opt/homebrew/bin/gfortran --flang-bin /opt/homebrew/bin/flang-new
```

Run one case with full stage capture:

```bash
cargo run -p afs-tests --bin bencch -- run --suite frontend --case stage_walk --all --verbose
```

Write machine-readable reports:

```bash
cargo run -p afs-tests --bin bencch -- run --suite modules --all --json-report reports/modules.json --markdown-report reports/modules.md
```

Run consistency coverage:

```bash
cargo run -p afs-tests --bin bencch -- run --suite consistency --all
```

Run differential coverage:

```bash
cargo run -p afs-tests --bin bencch -- run --suite differential
```

Reports are written under `reports/`.

`compare` now prints a short summary block with status, divergence
classification, basis, difference count, changed artifacts, and the backend
used on each side before any per-artifact diffs.

`introspect` now groups portable outputs like `asm`, `obj`, and `runtime`
separately from adapter extras like `armfortas.ir` and `armfortas.tokens` in
text, JSON, and Markdown output, and it now reports requested, captured, and
missing artifacts at the top of the report.

Environment overrides work too:

```bash
BENCCH_ARMFORTAS_BIN=./target/debug/armfortas cargo run -p afs-tests --bin bencch -- run --suite consistency/object
```

Backend choice is visible in:

- `cargo run -p afs-tests --bin bencch -- doctor`
- `--verbose` case runs
- JSON and Markdown reports as `primary_backend`
- bundle `metadata.txt` and `armfortas/metadata.txt`

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
- report outputs like `--json-report path/to/report.json` and `--markdown-report path/to/report.md`
- environment and adapter inspection with `doctor`
- direct one-shot compare with `compare`
- direct one-shot artifact/stage inspection with `introspect`

## Notes

- `.docs/` is local and gitignored.
- `bencch` is now the public CLI story; `afs-tests` remains as a compatibility
  alias.
- The product is now centered on `compare`, `introspect`, `run`, and `doctor`.
- The runner is currently strongest on stage capture, differential behavior,
  and consistency work around reproducibility and cross-path mismatches.

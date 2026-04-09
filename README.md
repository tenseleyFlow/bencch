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

CLI-side compiler and tool paths are overridable. Rich linked `armfortas`
capture still needs an `armfortas` checkout, but Sprint 13 now gives that a
real bootstrap path instead of assuming `bencch` is embedded as a submodule.

Embedded usage still works:

```bash
cargo run -p afs-tests --bin bencch -- list
cargo run -p afs-tests --bin bencch -- run --suite frontend
```

Standalone linked usage now works through a generated local workspace:

```bash
scripts/bootstrap-linked-armfortas.sh /path/to/armfortas
cargo run --manifest-path .bencch-local/Cargo.toml -p afs-tests --bin bencch -- doctor
```

That generated path keeps linked capture working and makes `doctor` report the
actual linked `armfortas` checkout instead of assuming `bencch` is embedded.

Standalone external-only usage now works through a second generated workspace:

```bash
scripts/bootstrap-standalone-external.sh
cargo run --manifest-path .bencch-external/Cargo.toml -p afs-tests --bin bencch -- doctor
```

That mode drops linked capture entirely and keeps the generic external-driver
surface available for `compare`, `introspect`, and external-facing `run` work.

Example external-only introspection:

```bash
cargo run --manifest-path .bencch-external/Cargo.toml -p afs-tests --bin bencch -- introspect fixtures/fake_compilers/match_42_a.sh fixtures/runtime/if_else.f90 --artifact asm,runtime
```

Example external-only authored suite run:

```bash
cargo run --manifest-path .bencch-external/Cargo.toml -p afs-tests --bin bencch -- run --suite v2/generic-introspect --case fake_compiler_runtime --all
```

Example external-only authored compare matrix:

```bash
cargo run --manifest-path .bencch-external/Cargo.toml -p afs-tests --bin bencch -- run --suite v2/generic-compare --case fake_compilers_match_matrix --all
```

Example external-only authored differential matrix:

```bash
cargo run --manifest-path .bencch-external/Cargo.toml -p afs-tests --bin bencch -- run --suite v2/generic-differential --all
```

Example external-only authored consistency matrix:

```bash
cargo run --manifest-path .bencch-external/Cargo.toml -p afs-tests --bin bencch -- run --suite v2/generic-consistency --all
```

Example external-only authored failure matrix:

```bash
cargo run --manifest-path .bencch-external/Cargo.toml -p afs-tests --bin bencch -- run --suite v2/generic-failure-matrix --case fake_compiler_expected_diagnostic_matrix --all
```

Legacy rich-stage suites still need linked capture. In an external-only build,
they now fail early with a direct message telling you to use
`scripts/bootstrap-linked-armfortas.sh`.

## Usage

List suites:

```bash
cargo run -p afs-tests --bin bencch -- list
```

List suites with case-level capability discovery:

```bash
cargo run -p afs-tests --bin bencch -- list --suite v2/generic --verbose
```

Run one suite family:

```bash
cargo run -p afs-tests --bin bencch -- run --suite consistency/runtime
```

Inspect the current embedded/standalone posture:

```bash
cargo run -p afs-tests --bin bencch -- doctor
```

`doctor` now also lists the generic artifacts and namespaced adapter extras
that each named compiler surface can provide in the current build.

The built-in named compiler set now includes `armfortas`, `gfortran`,
`flang-new`, `lfortran`, `ifort`, `ifx`, and `nvfortran` / `pgfortran`, plus
any explicit compiler path you pass to `compare` or `introspect`.

Write the same `doctor` snapshot to JSON and Markdown:

```bash
cargo run -p afs-tests --bin bencch -- doctor --json-report reports/doctor.json --markdown-report reports/doctor.md
```

The JSON report now includes structured sections for workspace, named compiler
surfaces, tools, and mode, while keeping the flat field map too.

Generate a local linked workspace against an external `armfortas` checkout:

```bash
scripts/bootstrap-linked-armfortas.sh /path/to/armfortas
```

Then run `bencch` through that generated workspace:

```bash
cargo run --manifest-path .bencch-local/Cargo.toml -p afs-tests --bin bencch -- list
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

Namespaced adapter artifacts are allowed in `compare` too, but only when both
compiler surfaces can actually provide them. If not, `bencch` fails early with
an explicit capability message.

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

Trim large introspection sections to a readable preview:

```bash
cargo run -p afs-tests --bin bencch -- introspect armfortas fixtures/runtime/if_else.f90 --all --max-artifact-lines 12
```

Keep only section summaries and omit artifact bodies:

```bash
cargo run -p afs-tests --bin bencch -- introspect armfortas fixtures/runtime/if_else.f90 --all --summary-only
```

Introspect a named external compiler on the generic surface:

```bash
cargo run -p afs-tests --bin bencch -- introspect gfortran fixtures/runtime/if_else.f90 --artifact asm,obj,runtime
```

If you request artifacts that a compiler surface cannot provide, `bencch`
fails early with a capability message instead of pretending the compiler
failed mid-pipeline.

Introspect an explicit compiler path on that same generic surface:

```bash
cargo run -p afs-tests --bin bencch -- introspect /path/to/compiler fixtures/runtime/if_else.f90 --artifact asm,obj,runtime
```

Introspect a failing armfortas source and keep the partial capture:

```bash
cargo run -p afs-tests --bin bencch -- introspect armfortas fixtures/invalid/parse_error.f90 --artifact armfortas.tokens,armfortas.ir,asm
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
missing artifacts at the top of the report. Failure-side introspection also
surfaces the failure stage when the adapter knows it, plus a short diagnostic
excerpt before the full diagnostics block. For large captures, `--summary-only`
and `--max-artifact-lines <n>` keep the text and Markdown surfaces readable.
JSON reports keep the full artifact bodies and now add compact
`artifact_summaries` alongside them for quick scanning.

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

The new suite-v2 generic surface can target any compiler spec the same way
`bencch introspect` does:

```text
suite "v2/generic-introspect"

case "fake_compiler_runtime_matrix"
source "../../fixtures/runtime/if_else.f90"
opts => O0, O1, O2
compiler "../../fixtures/fake_compilers/match_42_a.sh" => asm, runtime
expect asm contains ".globl _main"
expect run.stdout contains "42"
expect run.exit_code equals 0
end
```

Generic compiler cases can also lean on references and CLI-style
reproducibility checks:

```text
suite "v2/generic-differential"

case "gfortran_runtime_matrix"
source "../../fixtures/runtime/if_else.f90"
opts => O0, O1, O2
compiler gfortran => runtime
differential => flang-new
expect run.stdout check-comments
expect run.exit_code equals 0
end
```

```text
suite "v2/generic-consistency"

case "fake_compiler_runtime_matrix"
source "../../fixtures/runtime/if_else.f90"
opts => O0, O1, O2
repeat => 3
compiler "../../fixtures/fake_compilers/match_42_a.sh" => asm, runtime
consistency => cli_asm_reproducible, cli_run_reproducible
expect asm contains ".globl _main"
expect run.stdout contains "42"
expect run.exit_code equals 0
end
```

For mem2reg-branch compatibility, `check-comments` on `armfortas.ir` understands
inline `! IR_CHECK:` and `! IR_NOT:` annotations, while `run.stdout
check-comments` keeps using the usual `! CHECK:` lines.

Two more opt-in bridges exist for imported mem2reg-style audits:

- `expect-fail comments` reads `! ERROR_EXPECTED:` lines from the source
- `xfail comments` reads the first `! XFAIL:` line from the source

Those compose the same way the old mem2reg harness did: a case can keep a
source-owned expected diagnostic and still remain `xfail` until trunk starts
producing that diagnostic correctly.

Suite-v2 can also drive the generic compare engine:

```text
suite "v2/generic-compare"

case "fake_compilers_match_matrix"
source "../../fixtures/runtime/if_else.f90"
opts => O0, O1, O2
compare "../../fixtures/fake_compilers/match_42_a.sh" "../../fixtures/fake_compilers/match_42_b.sh" => asm
expect compare.status equals "match"
expect compare.classification equals "match"
expect compare.difference_count equals 0
end
```

Suite-v2 unhappy paths can use the same generic engine too:

```text
suite "v2/generic-failures"

case "fake_compiler_expected_diagnostic"
source "../../fixtures/invalid/fake_compile_fail_expected.f90"
compiler "../../fixtures/fake_compilers/compile_fail.sh" => diagnostics
expect-fail comments
end

case "armfortas_parse_error"
source "../../fixtures/invalid/parse_error.f90"
compiler armfortas => diagnostics
expect-fail parser contains "expected entity name"
end
```

And they can be matrixed the same way as the happy-path suites:

```text
suite "v2/generic-failure-matrix"

case "fake_compilers_compile_divergence_matrix"
source "../../fixtures/runtime/if_else.f90"
opts => O0, O1, O2
compare "../../fixtures/fake_compilers/compile_fail.sh" "../../fixtures/fake_compilers/match_42_a.sh" => diagnostics
expect compare.status equals "diff"
expect compare.classification equals "compile divergence"
expect compare.difference_count equals 2
end
```

If a suite-v2 case is intentionally blocked by adapter capability limits, you
can say so directly:

```text
suite "v2/capability-policy"

case "gfortran_armfortas_ir_future"
source "../../fixtures/runtime/if_else.f90"
compiler gfortran => armfortas.ir
future capability "generic gfortran surface has no armfortas extras"
end

case "mixed_surface_ir_compare_xfail"
source "../../fixtures/runtime/if_else.f90"
compare armfortas gfortran => armfortas.ir
xfail capability "mixed-surface namespaced compare stays soft for now"
end
```

Namespaced armfortas artifacts can be matrixed too:

```text
suite "v2/armfortas-namespace-matrix"

case "if_else_frontend_matrix"
source "../../fixtures/runtime/if_else.f90"
opts => O0, O1, O2
compiler armfortas => armfortas.tokens, armfortas.ast, armfortas.sema
expect armfortas.tokens contains "\"then\""
expect armfortas.ast contains "node: IfConstruct"
expect armfortas.sema contains "diagnostics: none"
end
```

Graph cases use `entry` plus ordered `file` lines:

```text
suite "v2/generic-graphs"

case "module_chain_frontend"
entry "../../fixtures/modules/module_chain/main.f90"
file "../../fixtures/modules/module_chain/math_seed.f90"
file "../../fixtures/modules/module_chain/math_values.f90"
file "../../fixtures/modules/module_chain/main.f90"
compiler armfortas => armfortas.ast, armfortas.sema
expect armfortas.ast contains "name: \"math_seed\""
expect armfortas.sema contains "local_name: \"doubled\""
end
```

Today the armfortas adapter materializes graph cases into one generated source
in declared file order before capture/compile. The authored files still stay in
the failure bundle.

Common things the runner understands:

- stage capture like `armfortas => tokens, ir, asm, obj, run`
- generic compiler capture like `compiler gfortran => asm, obj, runtime` or `compiler "/path/to/compiler" => asm, obj, runtime`
- suite-v2 generic compiler cases can also use opt matrices, `differential => ...`, and CLI-style reproducibility checks
- `check-comments` on `armfortas.ir` / `ir` uses `! IR_CHECK:` and `! IR_NOT:`
- `expect-fail comments` uses inline `! ERROR_EXPECTED:` source comments
- `xfail comments` uses the first inline `! XFAIL:` source comment
- generic compare cases like `compare gfortran flang-new => asm`, including opt matrices
- capability-aware authored softening with `future capability "..."` and `xfail capability "..."`
- suite-v2 graph cases with `entry` plus ordered `file` lines
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

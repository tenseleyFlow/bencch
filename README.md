# bencch

`bencch` is the extracted compiler-bench subtree that started life inside
`armfortas`.

Current extraction point: Sprint 6 differential and object/tool hardening.

That means this repo already carries:

- the `afs-tests` runner
- authored frontend, IR, opt, backend, object, runtime, and differential suites
- known-gap `xfail` coverage for stable armfortas divergences
- failure bundles and differential classification logic
- expanded green differential coverage for runtime, I/O, and interop cases
- explicit `-S` vs `-c` consistency coverage, with authored `xfail` cases for
  the cross-path object divergence that armfortas still shows today
- explicit reproducibility coverage for repeated `-S` and `-c` runs, which now
  shows the deeper issue is compile-to-compile nondeterminism in emitted text
  and object bytes rather than just one driver-path mismatch
- repeat-aware consistency diagnostics, so authored cases can say how many
  rebuilds to sample and the runner can report unique-variant counts plus which
  object components actually varied
- consistency-driven `XFAIL`s now emit real report bundles with copied repeated
  run artifacts and per-check summaries under `reports/consistency...`, instead
  of leaving the interesting evidence stranded in temporary directories
- end-of-run summaries now roll up consistency families by check, so larger
  nondeterminism suites collapse into compact `repeat_count`/`unique_variants`
  and varying/stable-component summaries instead of only a wall of individual
  cell output
- capture-vs-CLI triangulation coverage for both `asm` and `obj`, sampled over
  repeated CLI rebuilds so the bench can tell whether the library capture path
  itself stays aligned with driver output
- capture-path reproducibility coverage for both `asm` and `obj`, so the bench
  can prove whether `armfortas::testing` is stable on its own or is only
  drifting relative to the CLI
- runtime consistency coverage for repeated full CLI builds, repeated
  `armfortas::testing` `run` capture, and capture-vs-CLI behavior comparisons,
  which currently stays green on the bench-owned backend fixtures and narrows
  the observed nondeterminism below the sampled runtime surface
- expanded runtime consistency coverage over real behavioral programs including
  numerics, array/control-flow, derived types, fixed-length strings, file I/O,
  and `bind(c)` interop, which is also green across the sampled opt matrix and
  reinforces that the current nondeterminism has not yet leaked into observed
  runtime behavior on this initial corpus

## Layout

- `bench/`
  - Rust runner crate (`afs-tests`)
- `suites/`
  - Authored suite manifests
- `fixtures/`
  - Reusable fixture corpus

## Current Wiring

Today `bench/Cargo.toml` still points at a surrounding `armfortas` checkout when
`bencch` is used as a submodule inside that repo. Generalizing compiler adapters
for truly standalone use is a planned follow-on step, not something this
extraction commit pretends is already solved.

## Planning

`.docs/` is intentionally local and gitignored in this repo. That is where the
live sprint plans and audit notes should stay.

Next planned slice from this extraction point:

- nondeterminism-focused follow-up work so the bench can collapse several
  current `xfail` consistency families into one rooted compiler issue
- broader repeat-aware sampling once the current `repeat => 3` coverage has
  proven out as a practical default
- continued Sprint 6 differential expansion where behavior is stable enough to
  compare across compilers
- deeper consistency coverage around other adapter-local boundaries, now that
  CLI runtime reproducibility, capture runtime reproducibility, and
  capture-vs-CLI runtime behavior are all in place alongside the existing
  `asm`/`obj` families
- broader runtime-consistency corpus work around larger programs, more I/O
  modes, and higher-surface language features so the bench can keep testing how
  far the current “stable at runtime, unstable below it” diagnosis really holds
- report-surface follow-up beyond the current console/bundle rollups, once we
  want machine-readable or cross-run consistency summaries
- sharper standalone boundaries between the generic bench and armfortas-local
  compiler adapter code
- continued hardening toward a standalone public bench repo

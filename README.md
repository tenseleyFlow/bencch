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

- deeper object/tool triangulation beyond the first `-S` vs `-c` check
- continued Sprint 6 differential expansion where behavior is stable enough to
  compare across compilers
- sharper standalone boundaries between the generic bench and armfortas-local
  compiler adapter code
- continued hardening toward a standalone public bench repo

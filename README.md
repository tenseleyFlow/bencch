# bencch

`bencch` is the extracted compiler-bench subtree that started life inside
`armfortas`.

Current extraction point: Sprint 6 differential corpus expansion.

That means this repo already carries:

- the `afs-tests` runner
- authored frontend, IR, opt, backend, object, runtime, and differential suites
- known-gap `xfail` coverage for stable armfortas divergences
- failure bundles and differential classification logic
- expanded green differential coverage for runtime, I/O, and interop cases

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

- object/tool consistency checks (`-S` vs `-c`, system tools, relocation shape)
- continued Sprint 6 differential expansion where behavior is stable enough to
  compare across compilers
- continued hardening toward a standalone public bench repo

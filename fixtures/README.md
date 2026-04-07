# Bench Fixtures

This directory is the long-term home for bench-owned reusable inputs.

The first `afs-tests` slice still references the legacy `test_programs/`
corpus directly so we can migrate coverage into suites quickly without losing
the existing compiler smoke tests. New shared fixtures should land here, and
later sprints will move legacy inputs across when the suite layout settles.

The frontend corpus now lives under `fixtures/frontend/` with dedicated
subdirectories for `preprocess`, `lexer`, `parser`, and `sema`.

IR and optimizer suites currently lean on the shared `test_programs/` corpus
plus a few focused bench fixtures while the middle-end coverage grows.

Sprint 5 adds `fixtures/backend/` for backend-and-object-facing programs
that exercise machine IR, register allocation, assembly emission, wrapper
generation, and Mach-O snapshot assertions.

Sprint 6 begins carving stable runtime programs into `fixtures/runtime/` so
targeted suites can stop depending on the surrounding parent repo layout.
The `runtime-completion`, `runtime-control-flow`, and `runtime-stateful`
families now live there as bench-owned inputs, along with the
`runtime-behavior` slice (`mixed_types`, `where_construct`,
`derived_type_basic`, and `string_fixed`).

Sprint 7 begins carving authored module graphs into `fixtures/modules/` so the
bench can model ordered multi-file inputs instead of only isolated sources.
That corpus now covers module-use chains, renaming, module procedures, and an
early submodule probe, plus visibility and fan-in graph families.

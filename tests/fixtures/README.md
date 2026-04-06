# Bench Fixtures

This directory is the long-term home for bench-owned reusable inputs.

The first `afs-tests` slice still references the legacy `test_programs/`
corpus directly so we can migrate coverage into suites quickly without losing
the existing compiler smoke tests. New shared fixtures should land here, and
later sprints will move legacy inputs across when the suite layout settles.

The frontend corpus now lives under `tests/fixtures/frontend/` with dedicated
subdirectories for `preprocess`, `lexer`, `parser`, and `sema`.

IR and optimizer suites currently lean on the shared `test_programs/` corpus
plus a few focused bench fixtures while the middle-end coverage grows.

Sprint 5 adds `tests/fixtures/backend/` for backend-and-object-facing programs
that exercise machine IR, register allocation, assembly emission, wrapper
generation, and Mach-O snapshot assertions.

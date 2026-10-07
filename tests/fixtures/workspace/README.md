# Regression fixture

A synthetic pnpm workspace that `tests/regression.rs` scans. Each type is
planted to produce exactly one expected finding, or to be filtered out as
noise; the expectations table in the test lists them all.

Dependencies: `ui → api → core → schema`, `api → schema`, `web → schema`;
`evals` depends on nothing.

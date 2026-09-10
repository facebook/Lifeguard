# Stub files for lifeguard

Lifeguard stub files are `.pyi` files with additional effect annotations in
function and method bodies. Since typecheckers read function signatures and
ignore the bodies, this lets us potentially have a common set of stubs for both
types and effects.

Note that lifeguard only uses the pyi files for effects, keyed by the function
name. Type signatures are ignored, and overloads are merged into a single
function definition. In particular, we do not use the annotated return type,
which might lead to unexpected "unknown method call" errors.

## Third party code

In addition to the builtins and stdlib, we have stubs for a few projects under
`resources/stubs/shared/`. These stub files override the corresponding source
files (see `source_priority` in `source_map.rs` for details), and let us provide
effects directly in cases where the python code is hard to analyse.

## A note on overloads

Since we merge effects from all overloads anyway, we only add the effect
annotations to the first overload in a set, leaving all the other overloads as
`def f(): ...`. This helps keep the stub files more readable, and makes diffing
them against the original typeshed files easier. There is a helper script,
`resources/scripts/normalize_stubs.py`, which will rewrite the stubs to do this
overload merging if needed.

Use `no_effects()` only for a function with no other declared effects; the stub
analyzer rejects combining it with other effect kinds, even across overloads.

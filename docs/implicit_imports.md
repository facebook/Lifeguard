# Implicit Imports

## What Is an Implicit Import?

An **implicit import** occurs when Python code accesses a submodule through attribute access on a parent module, without having directly imported that submodule. This works in CPython because importing a submodule (e.g., `import foo.bar`) sets `bar` as an attribute on the `foo` module object as a side effect. Other modules that have already imported `foo` can then access `foo.bar` without importing it themselves.

Under lazy imports, this side effect may not have happened yet. If the import that would have loaded `foo.bar` has been deferred, accessing `foo.bar` raises an `AttributeError`.

**Example**:

```python
# __main__.py
import foo
import waldo
foo.bar  # <-- implicit import: foo.bar was never directly imported here

# waldo.py
import foo.bar  # this side effect makes foo.bar available on the foo module object
```

Here, `__main__` accesses `foo.bar` but never imports it directly. It relies on `waldo` having imported `foo.bar` as a side effect. Under lazy imports, if `waldo` hasn't been loaded yet when `foo.bar` is accessed, the access fails.

## How Lifeguard Handles Implicit Imports

When Lifeguard detects an implicit import in a module, it adds the implicitly-imported module to the `lazy_eligible` dict for that module. This tells the lazy import loader to eagerly load the implicit dependency when the module is loaded lazily, preventing the `AttributeError` at runtime.

For example, if `__main__` has an implicit import of `foo.bar`, the output `lazy_eligible` dict will include `"__main__": ["foo.bar"]`, instructing the loader to eagerly import `foo.bar` when `__main__` is loaded.

## Detection Algorithm

Implicit import detection is a **cross-module analysis** that runs after all individual modules have been analyzed. It cannot happen during per-module analysis because it requires knowledge of what each module imports and accesses across the entire project.

### Phase 1: Per-Module Data Collection

During AST traversal of each module (`source_analyzer.rs`), three data structures on `ModuleEffects` are populated:

- **`pending_imports`**: Maps scopes (module-level or function names) to the set of modules imported in that scope. Every `import X` or `from X import Y` statement adds entries here.
- **`called_imports`**: Maps scopes to modules that are actually used/accessed (not just imported). When code accesses `foo.bar` and `foo` is a known import, `foo.bar` is recorded as a called import.
- **`called_functions`**: Tracks which functions defined in the module are actually called at module level. This is used to determine whether a function call triggers an import.

### Phase 2: Cross-Module Detection

After all modules are analyzed in parallel, `get_implicit_imports()` in `project.rs` runs the detection algorithm. It orchestrates three steps:

1. **`build_init_module_map()`** — Pre-computes a mapping from base module names to their `__init__` modules (e.g., `foo` → `foo/__init__`).

2. **`get_additional_called_imports()`** — For each module, identifies cases where calling a function in one module triggers imports in another module. This is necessary because the analysis map is immutable after the parallel analysis phase.

3. **`compute_implicit_imports_for_module()`** — The core detection logic, run in parallel for every module. For each module, it starts by assuming **every called import is implicit**, then eliminates non-implicit ones by checking several conditions.

### Elimination Conditions

A called import is **not** implicit if any of the following hold:

1. **Direct import exists**: The import statement exists in the scope where the import is accessed, or at module level.

2. **Called function import**: The called import is actually a function that was called, and any imports within that function are considered loaded.

3. **Loaded through imported module**: The import was loaded indirectly through a function call in another imported module.

4. **Attribute of imported parent**: The called import is actually an attribute of an already-imported parent module (not a separate module). This is determined using the import graph to distinguish "module `foo.bar`" from "attribute `bar` on module `foo`".

5. **Import-as alias**: The import resolves through an alias (e.g., `import foo.bar.baz as baz`).

The final result is: `called_imports − non_implicit_imports = implicit_imports`.

### Phase 3: Output Integration

In `output.rs`, implicit imports are added to the `lazy_eligible` dict for passing modules. If module A is otherwise safe but has an implicit import of B, then B is added to A's `lazy_eligible` set.

## Examples

### Basic Implicit Import (Detected)

```python
# __main__.py — implicit import of foo.bar detected
import foo
import waldo
foo.bar  # accesses foo.bar without importing it

# waldo.py
import foo.bar  # side effect that makes foo.bar available
```

`__main__` has an implicit import of `foo.bar` because it relies on `waldo`'s side effect.

### Explicit Import (Not Implicit)

```python
# __main__.py — no implicit import
import foo.bar
foo.bar.Bar  # foo.bar was directly imported
```

No implicit import because `foo.bar` was explicitly imported.

### Import Inside Called Function (Not Implicit)

```python
# __main__.py — no implicit import
import foo.bar
foo.bar.foo_bar()
foo.bar.baz  # loaded by foo_bar()

# foo/bar.py
def foo_bar():
    import foo.bar.baz
```

Not implicit because `foo_bar()` is called at module level, triggering the import of `foo.bar.baz`.

### Import Inside Uncalled Function (Detected)

```python
# __main__.py — implicit import of foo.bar.baz detected
import foo.bar
import waldo
foo.bar.baz

# foo/bar.py
def foo_bar():         # never called
    import foo.bar.baz

# waldo.py
import foo.bar.baz
```

Implicit because `foo_bar()` is never called, so the import inside it doesn't execute. The access relies on `waldo`'s side effect.

### Import-As Alias in Parent (Not Implicit)

```python
# __main__.py — no implicit import
import foo.bar
foo.bar.foo_bar_baz
foo.bar.baz

# foo/bar.py
import foo.bar.baz as foo_bar_baz
```

Not implicit because `foo.bar` explicitly imports `foo.bar.baz` via an alias, and accessing `foo.bar.foo_bar_baz` triggers that import.

### Import in Try Block (Not Implicit)

```python
# __main__.py — no implicit import
import foo
import waldo
waldo
foo.bar

# waldo.py
try:
    import foo.bar
except:
    print("womp womp")
```

Not implicit because imports inside `try` blocks are treated as eagerly loaded (they execute at module level).

### Import in Parent Module Stays Lazy (Detected)

```python
# __main__.py — implicit import of foo.bar.baz detected
import foo.bar
foo.bar.baz

# foo/bar.py
import foo.bar.baz  # this import is itself lazy
```

Implicit because `foo.bar`'s import of `foo.bar.baz` is a top-level import that remains lazy. The import statement existing in `foo.bar` does not guarantee it has executed.

## Key Source Files

| File | Role |
|---|---|
| `src/module_effects.rs` | `ModuleEffects` struct with `pending_imports`, `called_imports`, `called_functions` |
| `src/source_analyzer.rs` | Per-module AST traversal that populates `ModuleEffects` |
| `src/project.rs` | `get_implicit_imports()` and `compute_implicit_imports_for_module()` — core detection |
| `src/module_safety.rs` | `ModuleSafety` struct storing detected `implicit_imports` per module |
| `src/output.rs` | Integration of implicit imports into the `lazy_eligible` output dict |
| `tests/port_test_catch_implicit_imports.rs` | Unit tests |

# Getting Started with Lifeguard

Lifeguard analyzes Python codebases to determine which modules can safely
use lazy imports without triggering side effects at import time. This guide
walks you through installing the tool from PyPI or building it from source.

## Install from PyPI

```bash
pip install lifeguard-lazy-imports
lifeguard run-tree /path/to/project output.json --verbose-output verbose.txt
```

Wheels are available for Linux, macOS, and Windows on Python 3.12 or newer, so
no Rust toolchain is needed. `python -m lifeguard_lazy_imports` is equivalent
to the `lifeguard` command. In the Cargo examples below, substitute `lifeguard`
for `cargo run --`; the bundled sample project is in the repository, not the package.

## Prerequisites for Building from Source

- **Rust (nightly)** — install via [rustup](https://rustup.rs/). Cargo selects
  the nightly pinned in [rust-toolchain.toml](rust-toolchain.toml) automatically.
- **Git** — needed to clone Lifeguard and its submodules.

## Clone & Setup

Lifeguard uses Git submodules (for pyrefly), so use `--recurse-submodules`:

```bash
git clone --recurse-submodules https://github.com/facebook/Lifeguard.git
cd Lifeguard
```

If you already cloned without the flag, initialize submodules afterwards:

```bash
git submodule update --init --recursive
```

Verify everything compiles:

```bash
cargo build
```

## Quick Start — Analyze a directory (`run-tree`)

The `run-tree` subcommand discovers `.py` files under a directory tree and
follows resolvable top-level imports. File and directory names below the input
root must be ASCII Python identifiers; other paths are skipped.

Run it against the bundled sample project:

```bash
cargo run -- run-tree testdata/sample_project output.json
```

Add `--verbose-output verbose.txt` to see per-module details:

```bash
cargo run -- run-tree testdata/sample_project output.json \
  --verbose-output verbose.txt
```

You will see output similar to:

```
Found 6 Python files
--- Lifeguard Analysis for testdata/sample_project ---
1, (ExecCall, "exec")
1, (UnsafeFunctionCall, "main.main")
1, (CustomFinalizer, "__del__")
PASS RATE BY FILE %    | AVG NUM OF ERRORS IN FAILING MODULES
50.00 %                | 1.00
Num of failing files: 3
Num of passing files: 3
Num of load-imports-eagerly modules: 2
Output written to output.json
Full time executing: 222.72ms
```

**Reading the output:**

| Metric | Meaning |
|--------|---------|
| Passing files | Modules that are safe for lazy imports |
| Failing files | Modules with side effects that prevent lazy imports |
| Load-imports-eagerly modules | Modules where *all* imports must be loaded eagerly (e.g. `exec()` calls, custom `__del__` finalizers) |

The JSON written to `output.json` contains these keys (the last two only with
`--verbose-output`):

```json
{
  "LOAD_IMPORTS_EAGERLY": [
    "has_finalizer",
    "uses_exec"
  ],
  "LAZY_ELIGIBLE": {
    "importer": [
      "safer_lazy_imports.lifeguard.testdata.sample_project.safe_module",
      "safer_lazy_imports.lifeguard.testdata.sample_project.safe_module.greet",
      "safer_lazy_imports.lifeguard.testdata.sample_project.unsafe_module",
      "safer_lazy_imports.lifeguard.testdata.sample_project.unsafe_module.helper"
    ],
    "pkg": [],
    "pkg.sub": [],
    "safe_module": [],
    "unsafe_module": []
  },
  "IMPLICIT_IMPORTS": {},
  "IMPORT_CYCLES": []
}
```

- **`LOAD_IMPORTS_EAGERLY`** — a list of modules where lazy imports are disabled
  entirely. These modules had constructs that make static analysis impossible
  (e.g. `exec()` calls, custom `__del__` finalizers). See
  `docs/load_imports_eagerly.md` for the exact cases.
- **`LAZY_ELIGIBLE`** — a dict mapping each safe module to the list of
  dependencies that must be imported eagerly when that module is loaded lazily.
  - An empty list (like `safe_module` above) means the module is fully safe
    with no caveats.
  - A non-empty list (like `importer` above) means the module
    is safe *except* those dependencies must be loaded eagerly.
  - A module can appear in both `LAZY_ELIGIBLE` and `LOAD_IMPORTS_EAGERLY`.
- **`IMPLICIT_IMPORTS`** and **`IMPORT_CYCLES`** — diagnostic details included
  only with `--verbose-output`. Both are empty in this sample run.

## Reading the verbose output

When you pass `--verbose-output verbose.txt`, Lifeguard writes a per-module
breakdown. Here is an example:

```
# Lifeguard Verbose Output:
------------------------------------------------------------------------------
## has_finalizer
### Errors
CustomFinalizer (1)
  Line 11 - __del__

### Load Imports Eagerly
CustomFinalizer (1)
  Line 11 - __del__

## importer
### Lazy imports incompatibilities were not detected

## main
### Errors
UnsafeFunctionCall (1)
  Line 21 - main.main

## safe_module
### Lazy imports incompatibilities were not detected

## unsafe_module
### Lazy imports incompatibilities were not detected

## uses_exec
### Errors
ExecCall (1)
  Line 8 - exec

### Load Imports Eagerly
ExecCall (1)
  Line 8 - exec
```

Each `##` heading is a module. Under it you will see:

- **"Lazy imports incompatibilities were not detected"** — no local safety
  errors, eager-import overrides, or implicit imports were reported. The JSON
  can still list dependency constraints, as it does for `importer` above.
- **Errors** — diagnostics grouped under `<ErrorKind> (<count>)`, followed by
  `Line <n> - <detail>` entries. Common error kinds:
  - `UnknownFunctionCall` — a call target could not be resolved
  - `UnsafeFunctionCall` — a local or imported function that was statically analyzed to be lazy imports incompatible
  - `CustomFinalizer` — a class defines `__del__`
  - `ExecCall` — the module calls `exec()`
- **Load Imports Eagerly** — errors that cause the module to be added to the
  `LOAD_IMPORTS_EAGERLY` set (its own imports must execute eagerly).
- **Implicit Imports** — submodule accesses that rely on another import having
  already loaded the submodule, such as `import foo; foo.bar` without a direct
  import of `foo.bar`. See [docs/implicit_imports.md](docs/implicit_imports.md).
- **Analysis Error** — the module could not be analyzed, for example because
  parsing failed.

Empty diagnostic sections are omitted.

## Running tests

```bash
cargo test
```

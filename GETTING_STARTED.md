# Getting Started with Lifeguard

Lifeguard analyzes Python codebases to determine which modules can safely
use lazy imports without triggering side effects at import time. This guide
walks you through building and running the tool from source using Cargo.

## Prerequisites

- **Rust (nightly)** — the crate uses unstable features, so you need a nightly
  toolchain. Install via [rustup](https://rustup.rs/) and set it with
  `rustup default nightly`.
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

The `run-tree` subcommand analyzes every `.py` file under a directory tree.

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
Full time executing (CPU): 1.39s
```

**Reading the output:**

| Metric | Meaning |
|--------|---------|
| Passing files | Modules that are safe for lazy imports |
| Failing files | Modules with side effects that prevent lazy imports |
| Load-imports-eagerly modules | Modules where *all* imports must be loaded eagerly (e.g. `exec()` calls, custom `__del__` finalizers) |

The JSON written to `output.json` contains two top-level keys:

```json
{
  "LOAD_IMPORTS_EAGERLY": [
    "has_finalizer",
    "uses_exec"
  ],
  "LAZY_ELIGIBLE": {
    "importer": [
      "safer_lazy_imports.lifeguard.testdata.sample_project.unsafe_module.helper",
      "safer_lazy_imports.lifeguard.testdata.sample_project.safe_module",
      "safer_lazy_imports.lifeguard.testdata.sample_project.safe_module.greet",
      "safer_lazy_imports.lifeguard.testdata.sample_project.unsafe_module"
    ],
    "safe_module": [],
    "unsafe_module": [
      "os.path"
    ]
  }
}
```

- **`LOAD_IMPORTS_EAGERLY`** — a list of modules where lazy imports are disabled
  entirely. These modules had constructs that make static analysis impossible
  (e.g. `exec()` calls, custom `__del__` finalizers). See
  `docs/excluding_set.md` for the exact cases.
- **`LAZY_ELIGIBLE`** — a dict mapping each safe module to the list of
  dependencies that must be imported eagerly when that module is loaded lazily.
  - An empty list (like `safe_module` above) means the module is fully safe
    with no caveats.
  - A non-empty list (like `unsafe_module` → `["os.path"]`) means the module
    is safe *except* those dependencies must be loaded eagerly.
  - Modules that appear in `LOAD_IMPORTS_EAGERLY` are omitted from this dict.

# Reading the verbose output

When you pass `--verbose-output verbose.txt`, Lifeguard writes a per-module
breakdown. Here is an example:

```
# Lifeguard Verbose Output:
------------------------------------------------------------------------------
## has_finalizer
### Errors
  Line 6 - CustomFinalizer __del__
### Load Imports Eagerly
  Line 6 - CustomFinalizer __del__
### Implicit Imports

## importer
### Lazy imports incompatibilities were not detected

## main
### Errors
  Line 10 - UnsafeFunctionCall main.main
### Load Imports Eagerly
### Implicit Imports

## safe_module
### Lazy imports incompatibilities were not detected

## unsafe_module
### Errors
### Load Imports Eagerly
### Implicit Imports
  os.path

## uses_exec
### Errors
  Line 3 - ExecCall exec
### Load Imports Eagerly
  Line 3 - ExecCall exec
### Implicit Imports
```

Each `##` heading is a module. Under it you will see:

- **"Lazy imports incompatibilities were not detected"** — the module is fully
  safe; nothing else to report.
- **Errors** — side effects detected at the listed lines. The format is
  `Line <n> - <ErrorKind> <detail>`. Common error kinds:
  - `CalledImport` — an imported function is called at module scope
  - `UnsafeFunctionCall` — a local function with side effects is called at
    module scope
  - `CustomFinalizer` — a class defines `__del__`
  - `ExecCall` — the module calls `exec()`
- **Load Imports Eagerly** — errors that cause the module to be added to the
  `LOAD_IMPORTS_EAGERLY` set (lazy imports fully disabled for this module).
- **Implicit Imports** — modules that are implicitly imported as a side effect
  of importing this module (e.g. `import os.path` implicitly imports `os`).

## Running tests

```bash
cargo test
```

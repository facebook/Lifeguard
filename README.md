# Lifeguard for Lazy Imports

[![PyPI - Version](https://img.shields.io/pypi/v/lifeguard-lazy-imports.svg)](https://pypi.org/pypi/lifeguard-lazy-imports/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

A fast static analysis tool to aid adoption of [Lazy Imports](https://peps.python.org/pep-0810/) in Python.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/lifeguard_logo_text_dark.png">
    <img alt="Lifeguard" src="docs/lifeguard_logo_text.png" width="420">
  </picture>
</p>

## What are Lazy Imports?

In Python, every `import` statement executes immediately when a module is loaded. This overhead is incurred regardless of whether that import is actually used. [PEP 810](https://peps.python.org/pep-0810/) introduces *explicit Lazy Imports* to Python, which defer the actual loading of a module until the imported name is first accessed. Lazy Imports can significantly reduce memory usage, startup times, and import overhead, especially in large codebases with deep dependency trees.

However, some Python patterns depend on imports executing immediately. For example:

- **Module-level side effects** — a module that registers a handler or modifies global state at import time will behave differently if that import is deferred.
- **The registry pattern** — a module that registers itself (e.g., adding to a global dict) when imported will silently fail to register under Lazy Imports.
- **`sys.modules` manipulation** — code that reads or writes `sys.modules` assumes prior imports have already executed.
- **Metaclasses and `__init_subclass__`** — class creation side effects may depend on imports being resolved.

Adapting an existing codebase to use Lazy Imports can be a daunting task, especially at scale. Lifeguard identifies these incompatible patterns so you can adopt Lazy Imports with confidence.

## How does Lifeguard work?
Lifeguard analyzes Python source files for a given project in parallel. It walks each module's AST to detect effects and maps Lazy-Imports-incompatible effects to errors. The analyzer takes a conservative approach towards its analysis: any module that cannot be programmatically determined to be safe to import lazily is marked unsafe by default.
This means Lifeguard will err on the side of marking potentially compatible modules as incompatible, leaving potential performance optimizations on the table in favor of production safety.

For a deeper look at the analysis pipeline and architecture, see [docs/architecture.md](docs/architecture.md).

## Project Stage: Beta
Lifeguard is in active development. We are aiming to be ready for general use by the [Python 3.15 final release](https://peps.python.org/pep-0790/).

### Items on our roadmap
- We've tested and support Python 3.12 and 3.14. Other versions may also work. To analyze the explicit `lazy import` syntax from [PEP 810](https://peps.python.org/pep-0810/), pass `--python-version 3.15`.
- We are actively developing a standalone linter output mode to help users identify which specific lines in their codebase are incompatible with Lazy Imports.
- We plan to add support for easy ingestion of Lifeguard's output to drive Lazy Imports enablement for advanced users (see [Using the Output](#using-the-output)).

## Install from PyPI

Lifeguard is published on [PyPI](https://pypi.org/project/lifeguard-lazy-imports/) with prebuilt wheels for Linux, macOS, and Windows (x86-64 and ARM64). It requires Python 3.12 or newer and no Rust toolchain:

```bash
pip install lifeguard-lazy-imports
lifeguard run-tree /path/to/project output.json --verbose-output verbose.txt
```

`python -m lifeguard_lazy_imports` is equivalent to the `lifeguard` command. The `cargo run --` examples below build and run the tool from source; with the installed package, replace `cargo run --` with `lifeguard`.
PyPI releases are cut manually and can lag behind the main branch. Run `lifeguard --help` to see what your installed version supports.

## Prerequisites for Building from Source

- **Rust (nightly)** — install via [rustup](https://rustup.rs/). Cargo uses the nightly pinned in [rust-toolchain.toml](rust-toolchain.toml); there is no need to change your global default toolchain.
- **Git** — clone with submodules: `git clone --recurse-submodules https://github.com/facebook/Lifeguard.git`

If you already cloned without `--recurse-submodules`, run `git submodule update --init --recursive`.

## Quick Start

The fastest way to try Lifeguard is the `run-tree` subcommand, which discovers `.py` files under a directory and follows resolvable top-level imports. File and directory names below the input root must be ASCII Python identifiers; other paths are skipped.

```bash
cargo run -- run-tree <INPUT_DIR> <OUTPUT_PATH>
```

For example, using the bundled sample project:

```bash
cargo run -- run-tree testdata/sample_project output.json
```

For a full walkthrough including interpreting the output, see [GETTING_STARTED.md](GETTING_STARTED.md).

## Running Lifeguard

For larger projects where you need more control, you can generate a *source DB* — a JSON file that tells Lifeguard the full set of Python files in your project and their module paths (see [Input Format](#input-format) for details). Follow these steps:

1. Generate the source DB. We provide a subcommand to start this file for you, but you may need to tune it by hand. (As the project matures, we hope to make this process smoother.)
```
cargo run -- gen-source-db <INPUT_DIR> <OUTPUT_PATH>
```

Optionally, if your project has library dependencies, you can point Lifeguard at your site-packages by adding a `lifeguard` section to your `pyproject.toml`:

```toml
[lifeguard]
site_packages = "/path/to/site-packages"
```

You can find out your site-packages path via `python -m site`. Both `gen-source-db` and `run-tree` read this section from `<INPUT_DIR>/pyproject.toml`. Relative `site_packages` paths are resolved against `INPUT_DIR`. You can override the setting with `--site-packages /path/to/site-packages`.

**Note:** Discovery follows top-level import statements and may not discover all dependencies, such as imports nested in functions or conditional blocks outside the input tree. If Lifeguard reports missing modules, you may need to manually add entries to the generated source DB. For explicit lazy syntax, pass `--python-version 3.15` to both source discovery and analysis.

2. Run Lifeguard in one of two modes:
   - **Default**: Prints a high-level analysis of your codebase (% of compatible files, top errors, etc.) and writes the JSON output to `OUTPUT_PATH`.
   ```
   cargo run -- <DB_PATH> <OUTPUT_PATH>
   ```
   - **Verbose mode**: Also writes a human-readable report showing which specific lines in each module cause incompatibility.
   ```
   cargo run -- <DB_PATH> <OUTPUT_PATH> --verbose-output <VERBOSE_OUTPUT_PATH>
   ```

**Example Verbose Output:**
```text
## example.module.foo
### Errors
ImportedModuleAssignment (1)
  Line 17 - sys

UnsafeFunctionCall (1)
  Line 38 - example.demo.unsafe_method
```

## Input Format

In some modes, Lifeguard requires a source DB — a JSON file mapping Python module paths to their locations on disk. The format is:

```json
{
  "build_map": {
      "foo/bar.py": "/local/usr/disk/foo/bar.py",
      "example/__init__.py": "/local/usr/disk/third-party/example/__init__.py"
  }
}
```

You can generate this automatically using `cargo run -- gen-source-db` (see [Running Lifeguard](#running-lifeguard)), or create it by hand.

## Output Format

Lifeguard writes a JSON file with two fields:

```json
{
    "LAZY_ELIGIBLE": {
        "module1": [],
        "module2": ["module3", "module4"],
        "module5": []
    },
    "LOAD_IMPORTS_EAGERLY": ["module5", "module99", "module100"]
}
```

With `--verbose-output`, the JSON also includes `IMPLICIT_IMPORTS` (a module-to-dependencies mapping) and `IMPORT_CYCLES` (lists of modules in each cycle). Use `--sorted-output` for deterministic ordering of these fields.

### `LAZY_ELIGIBLE`

A dictionary mapping modules that are safe for Lazy Imports to a list of their dependencies that must be imported eagerly. For example:
- `"module1": []` — `module1` is fully safe for Lazy Imports with no restrictions.
- `"module2": ["module3", "module4"]` — `module2` is safe for Lazy Imports, **but only if** `module3` and `module4` have already been imported.

**Important:** Modules that do *not* appear as keys in this dictionary have been analyzed as unsafe for Lazy Imports.

### `LOAD_IMPORTS_EAGERLY`

A set of modules where *all* imports within the module must be loaded eagerly. Lazy Imports is essentially temporarily disabled for these modules.
**Note the distinction:** other modules can still lazily import a module in the `LOAD_IMPORTS_EAGERLY` set, but when that module does load, its own `import` statements must execute immediately rather than being deferred.

This set is only used for specific corner cases:
- **Custom finalizers** (`__del__`) — unpredictable execution timing means imports must be available at finalization.
- **`exec()` calls** — dynamic code execution negates static analysis guarantees.
- **`sys.modules` access** — reading or writing `sys.modules` could depend on prior imports having already executed.

For more details, see [docs/load_imports_eagerly.md](docs/load_imports_eagerly.md).

## Using the Output

### As a standalone linter

Lifeguard can be used as a standalone linter to identify which specific lines in your codebase are incompatible with Lazy Imports. Run the analyzer with `--verbose-output` to get a human-readable report showing per-module errors with line numbers (see [Running Lifeguard](#running-lifeguard)). This lets you treat Lifeguard like a linter: run it in CI or locally, review the flagged lines, and fix them. In this manner, Lifeguard is used as a guide to safely enable Lazy Imports.

### To drive a lazy import loader

The JSON output is designed to drive a lazy import loader's filter function. In Python 3.15, [`sys.set_lazy_imports_filter()`](https://peps.python.org/pep-0810/#lazy-imports-filter) installs a callback that controls which imports are deferred and which are loaded eagerly. Lifeguard's output provides the data needed to build this filter — using `LAZY_ELIGIBLE` to identify safe modules and their constraints, and `LOAD_IMPORTS_EAGERLY` to identify modules that need all imports resolved upfront.

We plan to provide tooling for easy ingestion of Lifeguard's output ahead of the Python 3.15 release. This is a work in progress.

## Implementation
Lifeguard is implemented in Rust. We leverage [ruff](https://github.com/astral-sh/ruff) for AST traversal and re-use several crates from [pyrefly](https://github.com/facebook/pyrefly). We also extend `.pyi` stub files to annotate known side effects in third-party libraries — for example, marking that a particular module-level function call in a dependency has observable behavior. These stubs are stored in the `resources/` folder. See [resources/stubs/stubs.md](resources/stubs/stubs.md) for details on how effect annotations work alongside standard type stubs.

## License
By contributing to Lifeguard, you agree that your contributions will be licensed under the LICENSE file in the root directory of this source tree.

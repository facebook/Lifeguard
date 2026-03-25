# Lifeguard for Lazy Imports
A fast static analysis tool to aid adoption for [Lazy Imports](https://peps.python.org/pep-0810/) in Python.

## What are Lazy Imports?

In Python, every `import` statement executes immediately when a module is loaded. This overhead is incurred regardless of if that import is actually used. [PEP 0810](https://peps.python.org/pep-0810/) introduces *explicit Lazy Imports* to Python, which defer the actual loading of a module until the imported name is first accessed. Lazy Imports can significantly reduce memory usage, startup times, and import overhead, especially in large codebases with deep dependency trees.

However, some Python patterns depend on imports executing immediately. For example:

- **Module-level side effects** — a module that registers a handler or modifies global state at import time will behave differently if that import is deferred.
- **The registry pattern** — a module that registers itself (e.g., adding to a global dict) when imported will silently fail to register under Lazy Imports.
- **`sys.modules` manipulation** — code that reads or writes `sys.modules` assumes prior imports have already executed.
- **Metaclasses and `__init_subclass__`** — class creation side effects may depend on imports being resolved.

Adapting an existing codebase to use Lazy Imports can be a daunting task, especially at scale. Lifeguard identifies these incompatible patterns so you can adopt Lazy Imports with confidence.

## How does Lifeguard work?
Lifeguard analyzes Python source files for a given target in parallel. It walks each module's AST to detect effects and maps Lazy Imports incompatible effects to errors. The analyzer takes a conservative approach towards its analysis: any module that cannot be programmatically determined to be safe to import lazily is marked unsafe by default.
This means Lifeguard will err on the side of marking potentially compatible modules as incompatible, trading maximum performance for production safety.

For a deeper look at the analysis pipeline and architecture, see [docs/architecture.md](docs/architecture.md).

## Project Stage: Beta
Lifeguard is in active development. We are still putting on the finishing touches.

<details>
<summary>View what we have planned!</summary>

### Open items on our roadmap
- We still need to set up the necessary GitHub actions to fully support external contributors.
- We do not yet release to [PyPI](https://pypi.org/).
- We currently support up to Python 3.14. This means we do not yet support the [`lazy` keyword added in PEP-810](https://peps.python.org/pep-0810/) — but we fully intend to support this ahead of the 3.15 release.
- At this stage, we've tested Lifeguard against 3.12 and 3.14.
- We are actively developing a standalone linter output mode to help users identify which specific lines in their codebase are incompatible with Lazy Imports.
- We plan to add support for easy ingestion of Lifeguard's output to drive Lazy Imports enablement for advanced users (see [Using the Output](#using-the-output)).
</details>

## Prerequisites

- **Rust (nightly)** — the crate uses unstable features. Install via [rustup](https://rustup.rs/) and set with `rustup default nightly`.
- **Git** — clone with submodules: `git clone --recurse-submodules https://github.com/facebook/Lifeguard.git`

If you already cloned without `--recurse-submodules`, run `git submodule update --init --recursive`.

## Quick Start

The fastest way to try Lifeguard is the `run-tree` subcommand, which analyzes every `.py` file under a directory. No additional setup needed.

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

1. Generate the source DB. We provide a script to start this file for you, but you may need to tune it by hand. (As the project matures, we hope to make this process smoother.)
```
cargo run --bin gen_source_db <INPUT_DIR> <OUTPUT_PATH>
```

Optionally, if your project has library dependencies, you can point Lifeguard at your site-packages by adding a `lifeguard` section to your `pyproject.toml`:

```toml
[lifeguard]
site_packages = "/path/to/site-packages"
```

You can find out your site-packages path via `python -m site`. The `gen_source_db` binary reads this section automatically when generating the source DB.

**Note:** The script may not discover all of your project's dependencies. If Lifeguard reports missing modules, you may need to manually add entries to the generated source DB.

2. Run Lifeguard in one of two modes:
   - **Default**: Prints a high-level analysis of your codebase (% of compatible files, top errors, etc.) and writes the JSON output to `OUTPUT_PATH`.
   ```
   cargo run --bin analyzer <DB_PATH> <OUTPUT_PATH>
   ```
   - **Verbose mode**: Also writes a human-readable report showing which specific lines in each module cause incompatibility.
   ```
   cargo run --bin analyzer <DB_PATH> <OUTPUT_PATH> --verbose-output <VERBOSE_OUTPUT_PATH>
   ```

**Example Verbose Output:**
```text
## example.module.foo
### Errors
  Line 17 - ImportedModuleAssignment sys
  Line 38 - UnsafeFunctionCall example.demo.unsafe_method
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

You can generate this automatically using `cargo run --bin gen_source_db` (see [Running Lifeguard](#running-lifeguard)), or create it by hand.

## Output Format

Lifeguard writes a JSON file with two fields:

```json
{
    "LAZY_ELIGIBLE": {
        "module1": [],
        "module2": ["module3", "module4"]
        "module5": [],
    },
    "LOAD_IMPORTS_EAGERLY": ["module5", "module99", "module100"]
}
```

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

The JSON output is designed to drive a lazy import loader's filter function. In Python 3.15, [`importlib.util.lazy_import`](https://peps.python.org/pep-0810/) accepts a filter callback that controls which imports are deferred and which are loaded eagerly. Lifeguard's output provides the data needed to build this filter — using `lazy_eligible` to identify safe modules and their constraints, and `load_imports_eagerly` to identify modules that need all imports resolved upfront.

We plan to provide tooling for easy ingestion of Lifeguard's output ahead of the Python 3.15 release. This is a work in progress — stay tuned for updates.

## Implementation
Lifeguard is implemented in Rust. We leverage [ruff](https://github.com/astral-sh/ruff) for AST traversal and re-use several crates from [pyrefly](https://github.com/facebook/pyrefly). We also extend `.pyi` stub files to annotate known side effects in third-party libraries — for example, marking that a particular module-level function call in a dependency has observable behavior. These stubs are stored in the `resources/` folder. See [resources/stubs/stubs.md](resources/stubs/stubs.md) for details on how effect annotations work alongside standard type stubs.

## License
By contributing to Lifeguard, you agree that your contributions will be licensed under the LICENSE file in the root directory of this source tree.

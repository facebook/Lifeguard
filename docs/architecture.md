# Architecture

### Analysis Pipeline

The pipeline is orchestrated through `runner::process_source_map()` (shared by `main.rs` and `commands/run_tree.rs`):

1. **Load sources** - Parse the source DB JSON, load stubs, build `Sources` (`source_map.rs`)
2. **Build import graph + exports** - Extract import relationships and module exports in a single pass (`ImportGraph::make_with_exports`)
3. **Analyze modules** - Parallel per-module analysis to detect side effects (`project::run_analysis`)
4. **Generate output** - Compute import chains and safety verdicts (`LifeGuardAnalysis::new`)

AST parsing is on-demand — modules are parsed as needed during import graph construction and analysis, avoiding holding all ASTs in memory at once.

### How It Fits Together

- `source_map.rs` loads the source DB and provides the `ModuleProvider` trait for on-demand parsing
- `imports.rs` builds the `ImportGraph` and collects `Exports` in a single pass over all modules
- `project.rs` runs parallel per-module analysis (dispatching through `analyzer.rs` to `source_analyzer.rs` or `stub_analyzer.rs`), then merges results into `ProjectInfo` and computes safety verdicts
- `output.rs` walks the import graph to build the final `LifeGuardOutput` (lazy_eligible dict + load_imports_eagerly set)

### Key Modules

**Analysis core**:
- `source_analyzer.rs` - Main analysis engine for `.py` files, side-effect detection
- `stub_analyzer.rs` - Analyzer for `.pyi` stub files, parses effect annotations
- `project.rs` - Global analysis coordination, call graph traversal, safety verdicts
- `module_info.rs` - Combined DefinitionTable + ClassTable construction (single AST pass optimization)

**Pipeline orchestration**:
- `runner.rs` - Shared pipeline orchestration used by `main.rs` and `commands/run_tree.rs`

**AST traversal helpers**:
- `cursor.rs` - Tracks current scope during AST traversal (module → class → function)
- `bindings.rs` - Name resolution across scopes (`BindingsTable`)
- `imports.rs` - Import graph construction and resolution

**Effect tracking**:
- `effects.rs` - Effect types and EffectTable
- `module_effects.rs` - Per-module effect accumulation (`ModuleEffects`)

**Error and formatting**:
- `errors.rs` - Safety error types. These represent incompatibilities with Lazy Imports
- `module_safety.rs` - Per-module safety results (errors, force_imports_eager_overrides, implicit_imports)

**Supporting infrastructure**:
- `source_map.rs` - Buck source DB loading, parallel AST parsing with rayon
- `class.rs` - Class metadata extraction
- `exports.rs` - Module export detection
- `stubs.rs` - Bundled stub file support
- `output.rs` - `LifeGuardOutput` and `LifeGuardAnalysis` construction

**Utilities**:
- `builtins.rs` - Builtin function resolution (e.g., `list`, `open`, `eval`)
- `graph.rs` - Generic directed graph wrapping `petgraph::DiGraph`, cycle detection via Tarjan's SCC
- `manual_override.rs` - Hardcoded list of functions declared safe
- `module_parser.rs` - Module parsing abstraction
- `tracing.rs` - Simple timing utility
- `traits.rs` - Extension traits bridging lifeguard with pyrefly types

**Binary utilities**:
- `commands/run_tree.rs` - Subcommand to analyze a directory tree without Buck (reuses `gen_source_db`'s discovery)
- `commands/show_effects.rs` - Subcommand to dump effects for a single Python file
- `commands/gen_source_db.rs` - Subcommand to generate a source DB from a directory tree; owns the import-following discovery also used by `run-tree`

**Local pyrefly forks**:
- `pyrefly/definitions.rs` - Local fork of pyrefly's definitions module (with `LIFEGUARD:` markers)
- `pyrefly/globals.rs` - Local fork of pyrefly's globals module

### Safety Heuristics

These are critical design decisions affecting correctness:

- **Indexing imported objects**: Treated as SAFE (most don't override `__getitem__` unsafely)
- **Recursive function calls**: Treated as UNSAFE (cannot determine termination)
- **Unresolved function calls**: Treated as SAFE (most are builtins)
- **`exec()` calls**: Module marked as UNSAFE and added to load_imports_eagerly set (differs from original analyzer)
- **`sys.modules` access**: Module added to load_imports_eagerly set (subscript access and method calls depend on import ordering that lazy imports disrupts)

### Output Structure

The analyzer produces a `LifeGuardOutput` with two main components:

1. **`lazy_eligible`** (HashMap): Maps safe modules → set of failing dependencies that must be loaded eagerly
   - This is the primary mechanism for controlling lazy import behavior
   - If module A is safe but imports module B which is unsafe, A maps to {B}
   - The lazy import loader uses this to eagerly load specific imports within otherwise-safe modules

2. **`load_imports_eagerly`** (Set): Modules where ALL imports must be loaded eagerly
   - **This is used for specific corner cases where the module's own imports must have already executed:**
     - `CustomFinalizer` - classes with custom `__del__` implementations (unpredictable execution timing)
     - `ExecCall` - modules that call `exec()` (negates static analysis)
     - `SysModulesAccess` - modules that access `sys.modules` (depends on other imports already being in `sys.modules`)
   - **Do NOT use `load_imports_eagerly` for general "unsafe module" handling** - that's what the `lazy_eligible` dict is for
   - The `load_imports_eagerly` set tells the loader to completely disable lazy import behavior for that module's imports

**Key distinction**: To mark a third-party module as unsafe for lazy imports, it should appear in the `lazy_eligible` dict values (as a failing dependency), NOT in the `load_imports_eagerly` set.

**Import cycle handling**: Import cycles are detected and handled — modules in import cycles have all cycle members added to their `lazy_eligible` set.

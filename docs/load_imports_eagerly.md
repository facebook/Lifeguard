# The LOAD_IMPORTS_EAGERLY Set
The **LOAD_IMPORTS_EAGERLY set** is one of the data structures in the output of the Lifeguard analyzer. When a module is added to this set, the lazy import loader completely disables lazy import behavior for all of the imports in that module.

The LOAD_IMPORTS_EAGERLY set is distinct from the LAZY_ELIGIBLE dict:
- **LAZY_ELIGIBLE dict**: Maps safe modules to specific unsafe dependencies that must already be imported for the key module to be loaded lazily. This is the more commonly utilized mechanism for handling most lazy imports incompatible behavior.
- **LOAD_IMPORTS_EAGERLY set**: Disables lazy imports entirely within a module. A module can both be safe to load lazily and in the LOAD_IMPORTS_EAGERLY set.

## The LOAD_IMPORTS_EAGERLY Cases
A module is added to the LOAD_IMPORTS_EAGERLY set when any of these cases are detected anywhere in the module, regardless of scope or reachability from top-level code.

### 1. Custom Finalizers
**Trigger**: A class defines a `__del__` method.

**Why this may be unsafe**: Custom finalizers (`__del__`) have unpredictable execution timing. If a lazily-imported module defines a class with `__del__`, the finalizer may run at an unexpected point during interpreter shutdown or garbage collection, potentially accessing modules that haven't been loaded. In order to ensure that imports are accessible at finalization, we eagerly load imports in modules defining `__del__`.

**Python example**:

```python
class ResourceHandler:
    def __del__(self):
        # Custom finalizer with unpredictable timing
        self.cleanup()
```

### 2. Exec Calls

**Trigger**: A call to the `exec()` builtin anywhere in the module.

**Why this may be unsafe**: `exec()` executes arbitrary code at runtime, which negates the guarantees of static analysis. The analyzer cannot determine what side effects the executed code might have, so the entire module is conservatively added to the LOAD_IMPORTS_EAGERLY set.
Note that this applies to **all** scopes (not just module-level), so an `exec()` call inside a nested function still triggers eager loading.

**Python examples**:

```python
config_code = load_config_string()
exec(config_code)  # Static analysis cannot reason about this
```

### 3. sys.modules Access

**Trigger**: A subscript access (`sys.modules["x"]`) or method call (`sys.modules.setdefault(...)`, `sys.modules.pop(...)`) on `sys.modules` at any scope, subject to the read exceptions below.

For subscript reads with a literal string key, the analyzer suppresses this
effect when the key names the current module, one of its parent packages, or
`__main__`, `builtins`, or `sys`. It also suppresses the effect when an enclosing
`try` handler explicitly names `KeyError`, or the module contains a recognized
`"key" in sys.modules` / `"key" not in sys.modules` test for that key.
Membership tests are collected across the module; this is a heuristic, not a
proof that a test guards a particular read. Aliases such as `import sys as s`
are not recognized by that membership-test scan.

These exceptions do not apply to subscript writes, deletions, or method calls.
A bare `except:` or `except Exception:` alone does not exempt a subscript read.

**Why this may be unsafe**: `sys.modules` is a runtime dict that reflects which modules have been loaded. Code that reads or writes `sys.modules` typically depends on other imports having already executed. Under lazy imports, `import foo` is deferred, so a subsequent `sys.modules["foo"]` will raise `KeyError` because the module hasn't actually been loaded yet. To ensure all imports are present in `sys.modules` when the module accesses it, we eagerly load imports in modules that access `sys.modules`.

**Python examples**:

```python
import sys
import signals.ahp.draftcrew.constants

# This fails under lazy imports because the import above was deferred
sys.modules.setdefault("constants", sys.modules["signals.ahp.draftcrew.constants"])

# These also trigger eager loading
sys.modules["my.module"]         # subscript read
sys.modules["alias"] = my_mod    # subscript write
sys.modules.pop("old_module")    # method call
```

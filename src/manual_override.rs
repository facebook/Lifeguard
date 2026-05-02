/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Manually add safety annotations to some functions.

// This lets us add manual annotations for some widely used functions without
// adding a stub file for their entire module. This is a bit of a hack, and
// should at the least be replaced by some mechanism for a list of safe
// functions loaded at runtime, so that users can do it on a per-project basis
// in open source code.

use std::sync::LazyLock;

use ahash::AHashSet;
use pyrefly_python::module_name::ModuleName;

/// Functions that are always treated as safe.
const SAFE_FUNCTIONS_ARRAY: &[&str] = &[
    // Functions that implement lazy importing, which means they are by definition safe.
    "_plotly_utils.importers.relative_import",
    "libfb.py.lazy_import.lazy_import",
    // Decorators for caching values.  These are akin to functools.cache().
    "f3.utils.decorators.cache",
    "libfb.py.decorators.lazy_property",
    "libfb.py.decorators.memoize_forever",
    "libfb.py.decorators.memoize_timed",
    // Functions that use the ABCMeta registry, which is hard to analyze but it is safe.
    "collections.abc.Mapping.register",
    "collections.abc.Sequence.register",
    "collections.abc.Set.register",
    // The application code overrides `logger.warning` which triggers an unsafe
    // assignment error. Manual inspection proves this to be innocuous.
    "bigcode.bcf.transformers.src.transformers.utils.logging.get_logger",
    // Mostly a wrapper over logging.getLogger().
    "f3.logging.basic_logger.F3Logger",
    // TODO: Totally safe, but stubs can't seem to identify it.
    "os.environ.get",
    // This is a decorator that given an expected class structure allows for the
    // user to apply state information by overriding the `__init__`
    // While confusing...this seems encapsulated and not incompatible
    "oci.decorators.init_model_state_from_kwargs",
    // This is a generated function so lifeguard cannot find its source. Since
    // it decorates a function in the current test module, it should not
    // have any lazy imports implications.
    "pytest.mark.parametrize",
    // This seems to be the same util as bigcode.bcf above
    "transformers.utils.logging.get_logger",
    // Meta-decorator that wraps functions while preserving signatures. Its body
    // only mutates locally-created objects (the wrapper closure).
    "decorator.decorator",
    // Feature detection via importlib.metadata.version(). Mutates a module-level
    // cache dict but has no external side effects.
    "dns._features.have",
    // Pure function that parses text into a dns.name.Name object. Transitively calls
    // ContextVar.set()/reset() which are currently UnknownEffects in the stub.
    "dns.name.from_text",
    // Codec constructors that only store parameters in instance attributes.
    "dns.name.IDNA2003Codec",
    "dns.name.IDNA2008Codec",
    "dns.name.Name",
];

/// Copy of SAFE_FUNCTIONS_ARRAY as a global set for faster searching.
static SAFE_FUNCTIONS: LazyLock<AHashSet<&str>> =
    LazyLock::new(|| AHashSet::from_iter(SAFE_FUNCTIONS_ARRAY.iter().cloned()));

/// Check if a function is overridden to be safe in the eyes of the analyzer.
pub fn declared_safe(func: &ModuleName) -> bool {
    SAFE_FUNCTIONS.contains(&func.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_lib::*;

    #[test]
    fn test_name_check() {
        let name = ModuleName::from_str("libfb.py.lazy_import.lazy_import");
        assert!(declared_safe(&name));
    }

    #[test]
    fn test_unsafe_libfb_lazy_import() {
        // Construct a deliberately unsafe libfb.py.lazy_import and show that it doesn't raise any
        // errors when analyzed.
        let libfb = r#"
            def lazy_import(foo):
                raise ValueError("what is this foo anyway?!")
        "#;
        let code = r#"
            from libfb.py.lazy_import import lazy_import
            lazy_import("foo")
        "#;
        check_all(vec![("code", code), ("libfb.py", libfb)]);
    }
}

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

use pyrefly_python::module_name::ModuleName;

use crate::hasher::AHashSet;

/// Functions that are always treated as safe.
const SAFE_FUNCTIONS_ARRAY: &[&str] = &[
    // Functions that implement lazy importing, which means they are by definition safe.
    "_plotly_utils.importers.relative_import",
    "libfb.py.lazy_import.lazy_import",
    // Decorators for caching values.  These are akin to functools.cache().
    "f3.utils.decorators.cache",
    "libfb.py.decorators.lazy_property",
    "libfb.py.decorators.memoize_fast",
    "libfb.py.decorators.memoize_fast_0",
    "libfb.py.decorators.memoize_forever",
    "libfb.py.decorators.memoize_timed",
    "libfb.py.memoize.memoize_fast",
    "libfb.py.memoize.memoize_fast_0",
    "libfb.py.memoize.memoize_timed",
    "psutil._common.memoize",
    "psutil._common.memoize_when_activated",
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
    // Pure wrapper: reads __qualname__/__module__, creates an idempotent logger,
    // returns a functools.wraps closure. No I/O or state mutation at decoration time.
    "libfb.py.asyncio.decorators.retryable",
    "libfb.py.asyncio.decorators.memoize_timed",
    // Constructs SpanScope storing name + deferred tracer lambda, wraps with
    // functools.wraps. Span is only started at call time. Explicitly designed
    // for module-level decoration before configure() is called.
    "tracing.span",
    // Async equivalent of functools.lru_cache. At decoration time only copies
    // function metadata and initializes an empty OrderedDict. No I/O or event
    // loop interaction until the decorated function is actually called.
    "async_lru.alru_cache",
    // Pysa marker decorator: wraps with a trivial pass-through via functools.wraps.
    // No registration, no global state, purely for static taint analysis.
    "confucius.analects.base.agentic_function.agent_function",
    // Pure functools.wraps wrapper that shields an async task. All asyncio work
    // (copy_context, create_task, shield) happens at call time, not decoration.
    "langchain_core.callbacks.manager.shielded",
    // Decorator factory that validates args and returns a closure. For FieldInfo
    // objects it just copies the field with an updated docstring. No warnings
    // emitted at decoration time — only at access/call time for properties/functions.
    "langchain_core._api.deprecation.deprecated",
    // Pydantic beta/deprecation markers — same pattern as deprecated above.
    "langchain_core._api.beta_decorator.beta",
    // langchain_core.prompts uses PEP 562 __getattr__ for lazy re-exports. The
    // source exists in the DB but the analyzer can't follow the dynamic dispatch.
    // All are pure data constructors building template objects from strings.
    "langchain_core.prompts.ChatPromptTemplate.from_messages",
    "langchain_core.prompts.ChatPromptTemplate.from_template",
    "langchain_core.prompts.PromptTemplate.from_template",
    "langchain_core.prompts.PromptTemplate.from_file",
    "langchain_core.prompts.prompt.PromptTemplate.from_template",
    "langchain_core.prompts.prompt.PromptTemplate.from_file",
    "langchain_core.prompts.chat.ChatPromptTemplate.from_messages",
    "langchain_core.prompts.chat.HumanMessagePromptTemplate.from_template",
    "langchain_core.prompts.chat.SystemMessagePromptTemplate.from_template",
    // attrs class decorator: generates __init__/__repr__/etc via class introspection.
    // No global state mutation, no I/O. Same category as @dataclass.
    "attr.s",
    "attr.attrs",
    // pytest hookimpl: pure marker decorator that tags a function for the plugin
    // system. Registration happens later when pytest collects plugins, not at
    // decoration time.
    "_pytest.config.hookimpl",
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
    "google.auth._helpers.copy_docstring",
    // Pure-wrapper deprecation decorators. At decoration time they only build a
    // closure that emits a DeprecationWarning at call time; no module-level
    // registry, no I/O. Same shape as langchain_core._api.deprecation.deprecated.
    "markdown.util.deprecated",
    // Pure functools.wraps wrapper. All tracing work happens inside the async
    // wrapper at call time, never at decoration time.
    "model_context_protocol.common.decorators.mcp_client_connect_decorator.mcp_client_connect",
    "nltk.internals.deprecated",
    "sympy.utilities.decorator.deprecated",
    // sandcastle test gate: at decoration time it either returns the function
    // unchanged or wraps it via functools.wraps. It mutates only the decorated
    // function's own __unittest_skip__/__unittest_skip_why__ attributes, never
    // module-level state.
    "torch.testing._internal.common_utils.skip_but_pass_in_sandcastle_if",
    // Docstring rewriter for the HuggingFace AutoConfig table. Reads from
    // module-level CONFIG_MAPPING_NAMES/MODEL_NAMES_MAPPING constants (no mutation)
    // and assigns to fn.__doc__ on the decorated function only.
    "transformers.models.auto.configuration_auto.replace_list_option_in_docstrings",
    "artillery.artillery2.api.cython.agent_tracing.devmate.decorators.devmate_trace_action",
    // Runs an async function synchronously.
    "libfb.py.asyncio.await_utils.await_sync_decorator",
    // Retry decorator factory.
    "fblearner.flow.util.python_utils.call_with_retries",
    //  pure functools.wraps closure
    "devai.src.core.model.experimental.base.reclassify_class_errors",
    // tenacity retry decorator factory. At decoration time it only constructs a
    // Retrying/AsyncRetrying controller from the captured args and returns a
    // functools.wraps wrapper (BaseRetrying.wraps). No global mutation, I/O, or
    // registration happens until the decorated function is actually called.
    "tenacity.retry",
    // Pure logger factory: returns a memoized logging.getLogger(...). Reads
    // sys.argv/__file__ only; no module-state mutation, I/O, or registration.
    "datainfra.spark.common.logger.get_logger",
    // Pure functools.wraps decorator factory: builds sync/async wrapper closures
    // that log the wrapped function's return value at call time. Nothing runs at
    // decoration time beyond closure construction.
    "feed_ranking_infra.vanguard.logtools.log_return_value",
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

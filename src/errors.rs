/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use anyhow::Result;
use anyhow::anyhow;
use ruff_text_size::TextRange;
use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use starlark_map::Equivalent;
use static_interner::Intern;
use static_interner::Interner;

use crate::effects::Effect;
use crate::effects::EffectData;
use crate::effects::EffectKind;
use crate::format::ErrorString;
use crate::format::bare_string;

static METADATA_INTERNER: Interner<String> = Interner::new();

#[derive(Hash, Eq, PartialEq)]
struct StrRef<'a>(&'a str);

impl Equivalent<String> for StrRef<'_> {
    fn equivalent(&self, key: &String) -> bool {
        self.0 == key
    }
}

impl From<StrRef<'_>> for String {
    fn from(value: StrRef<'_>) -> Self {
        value.0.to_owned()
    }
}

#[derive(
    Debug,
    Eq,
    PartialEq,
    PartialOrd,
    Ord,
    Hash,
    Copy,
    Clone,
    Serialize,
    Deserialize
)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    /// Decorator is being called eagerly and it's not known to be safe.
    UnsafeDecoratorCall,
    /// Function is being called eagerly and it's not known to be safe.
    UnsafeFunctionCall,
    /// Method is being called eagerly and it's not known to be safe.
    UnsafeMethodCall,

    /// Like UnsafeFunctionCall.  This is a historical artifact from the older analyzer.  It should
    /// be merged in with UnsafeFunctionCall in the future.
    ProhibitedCall,

    /// Decorator is being called and we haven't resolved it to a value.
    UnknownDecoratorCall,
    /// Function is being called and we haven't resolved it to a value.
    UnknownFunctionCall,
    /// Method is being called and we haven't resolved it to a value.
    UnknownMethodCall,
    /// Object is being accessed and we haven't resolved it to a value.
    UnknownObject,

    /// An exception is being explicitly raised but it is not being handled.
    UnhandledException,

    /// A class has a custom __del__ implementation.  This is a function call that we can't track
    /// statically, there's too many possible places where it could be run.
    CustomFinalizer,

    /// exec() is being called which negates any analysis we have about the current module.
    ExecCall,

    /// sys.modules is being accessed at module level, which depends on import
    /// ordering that lazy imports disrupts.
    SysModulesAccess,

    /// `__subclasses__()` is being called. A class only joins that list once its
    /// defining module has executed, so the answer depends on what was imported.
    SubclassesAccess,

    /// An attribute on an imported module is explicitly being assigned, mutating the other module.
    ImportedModuleAssignment,

    /// A variable being passed to a function call comes from an import.  The function could modify
    /// this variable.
    ImportedVarArgument,

    /// Used by stubs to indicate that a function is unsafe, if we don't have a more specific effect
    /// to annotate it with.  Unknown effects are always treated as safety errors.
    UnknownEffects,

    /// A call has more than MAX_ARGS positional arguments, exceeding the tracking bitset.
    TooManyArgs,

    /// `builtins.__import__` is replaced or removed. The loader calls it for every
    /// deferred import, so this module's own imports must resolve before it is installed.
    BuiltinsImportOverride,

    /// The module's own `.pyi` declares public names and its source binds none of them: the
    /// module is a loader whose imports fill it at import time, so those imports must run.
    NamesOnlyInStub,
}

impl ErrorKind {
    // An error anywhere in a module triggers its addition to `load_imports_eagerly`.
    pub fn requires_eager_loading_imports(&self) -> bool {
        matches!(
            self,
            Self::CustomFinalizer
                | Self::ExecCall
                | Self::SysModulesAccess
                | Self::SubclassesAccess
                | Self::BuiltinsImportOverride
                | Self::NamesOnlyInStub
        )
    }

    /// Whether this error kind can be a false positive when analyzed without
    /// dependencies. Unknown* means the callee wasn't resolved; Unsafe* means
    /// it was resolved via an import binding but effects defaulted to unsafe
    /// because the source module was missing.
    pub fn could_be_caused_by_missing_import(&self) -> bool {
        matches!(
            self,
            Self::UnknownFunctionCall
                | Self::UnknownMethodCall
                | Self::UnknownDecoratorCall
                | Self::UnknownObject
                | Self::UnsafeFunctionCall
                | Self::UnsafeMethodCall
                | Self::UnsafeDecoratorCall
        )
    }
}

impl ErrorString for ErrorKind {
    fn error_string(&self) -> String {
        bare_string(&self)
    }
}

/// Metadata for safety errors - an interned string
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ErrorMetadata(Intern<String>);

impl ErrorMetadata {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ErrorMetadata {
    fn from(s: &str) -> Self {
        Self(METADATA_INTERNER.intern(StrRef(s)))
    }
}

impl FromStr for ErrorMetadata {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(s))
    }
}

impl Serialize for ErrorMetadata {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl fmt::Display for ErrorMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A safety error, in both its in-process and its cached form.
#[derive(Debug, Clone, Copy)]
pub struct SafetyError {
    pub kind: ErrorKind,
    pub metadata: ErrorMetadata,
    /// Where the error is, for the verbose output to resolve against the source.
    /// In process only: a cache carries no positions, so a decoded error has the
    /// default range.
    pub range: TextRange,
    /// True when this error is from a parameterized decorator (`@deco(...)`),
    /// whose returned wrapper also runs at decoration time.
    pub parameterized_decorator: bool,
    /// Produced inside an `if __name__ == "__main__":` body. The reduce drops it
    /// for every module that is not the binary's main module.
    pub from_main_guard: bool,
}

impl SafetyError {
    /// Whether the callee is applied as a decorator, either bound or not.
    pub(crate) fn is_decorator_call(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::UnsafeDecoratorCall | ErrorKind::UnknownDecoratorCall
        )
    }

    pub fn new(kind: ErrorKind, metadata: String, range: TextRange) -> Self {
        Self {
            kind,
            metadata: metadata.parse().unwrap(),
            range,
            parameterized_decorator: false,
            from_main_guard: false,
        }
    }

    /// The same error, attributed to a `__main__` guard body.
    pub fn from_main_guard(mut self) -> Self {
        self.from_main_guard = true;
        self
    }

    pub fn new_from_effect(kind: ErrorKind, eff: &Effect) -> Self {
        Self {
            kind,
            metadata: eff.name.as_str().parse().unwrap(),
            range: eff.range,
            parameterized_decorator: is_parameterized_decorator_effect(eff),
            from_main_guard: eff.from_main_guard,
        }
    }

    // The error kind an effect reports as, without constructing the error.
    pub(crate) fn error_kind_for(kind: EffectKind) -> Option<ErrorKind> {
        match kind {
            EffectKind::ProhibitedFunctionCall => Some(ErrorKind::ProhibitedCall),
            EffectKind::UnknownFunctionCall => Some(ErrorKind::UnknownFunctionCall),
            EffectKind::Raise => Some(ErrorKind::UnhandledException),
            EffectKind::CustomFinalizer => Some(ErrorKind::CustomFinalizer),
            EffectKind::ExecCall => Some(ErrorKind::ExecCall),
            EffectKind::SysModulesAccess => Some(ErrorKind::SysModulesAccess),
            EffectKind::SubclassesAccess => Some(ErrorKind::SubclassesAccess),
            EffectKind::BuiltinsImportOverride => Some(ErrorKind::BuiltinsImportOverride),
            EffectKind::UnknownDecoratorCall => Some(ErrorKind::UnknownDecoratorCall),
            EffectKind::UnknownEffects => Some(ErrorKind::UnknownEffects),
            EffectKind::UnknownObject => Some(ErrorKind::UnknownObject),
            EffectKind::Unsafe => Some(ErrorKind::UnsafeFunctionCall),
            EffectKind::TooManyArgs => Some(ErrorKind::TooManyArgs),
            _ => None,
        }
    }

    // Some effects can be converted directly into safety errors.
    pub fn from_effect(eff: &Effect) -> Option<Self> {
        Self::error_kind_for(eff.kind).map(|kind| Self::new_from_effect(kind, eff))
    }

    pub fn from_unsafe_call(eff: &Effect) -> Result<Self> {
        let kind = match eff.kind {
            EffectKind::DecoratorCall | EffectKind::ImportedDecoratorCall => {
                ErrorKind::UnsafeDecoratorCall
            }
            EffectKind::FunctionCall | EffectKind::ImportedFunctionCall => {
                ErrorKind::UnsafeFunctionCall
            }
            EffectKind::MethodCall
            | EffectKind::UnboundMethodCall
            | EffectKind::SuperMethodCall
            | EffectKind::ImportedTypeAttr => ErrorKind::UnsafeMethodCall,
            _ => return Err(anyhow!("Unexpected call effect {:?}", eff)),
        };
        Ok(Self::new_from_effect(kind, eff))
    }
}

/// Whether `eff` is a decorator applied as a call (`@deco(...)`) rather than a
/// bare `@deco`. Only the call form runs a returned wrapper at decoration time,
/// so its nested functions must also be verified safe.
pub(crate) fn is_parameterized_decorator_effect(eff: &Effect) -> bool {
    matches!(
        eff.kind,
        EffectKind::DecoratorCall | EffectKind::ImportedDecoratorCall
    ) && matches!(eff.data, EffectData::Call(_))
}

/// Wire form for [`SafetyError`]: the fields a cache carries. The range is not
/// one of them, and the interned metadata has to round-trip through a plain
/// string.
#[derive(Deserialize)]
struct SerializedSafetyError {
    kind: ErrorKind,
    metadata: String,
    parameterized_decorator: bool,
    from_main_guard: bool,
}

/// The same shape, borrowed, for the write path. The metadata is interned, so
/// the owned form would allocate a `String` per error every time a cache is
/// written; `&str` and `String` serialize identically, so the bytes are the
/// same. Field order must stay in step with [`SerializedSafetyError`]: the wire
/// format is not self-describing.
#[derive(Serialize)]
struct SerializedSafetyErrorRef<'a> {
    kind: ErrorKind,
    metadata: &'a str,
    parameterized_decorator: bool,
    from_main_guard: bool,
}

impl Serialize for SafetyError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SerializedSafetyErrorRef {
            kind: self.kind,
            metadata: self.metadata.as_str(),
            parameterized_decorator: self.parameterized_decorator,
            from_main_guard: self.from_main_guard,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafetyError {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = SerializedSafetyError::deserialize(deserializer)?;
        Ok(Self {
            kind: wire.kind,
            metadata: ErrorMetadata::from(wire.metadata.as_str()),
            // Not carried: a cached offset would tie the bytes to where in the
            // file the error sits, and nothing downstream of a cache reads one.
            range: TextRange::default(),
            parameterized_decorator: wire.parameterized_decorator,
            from_main_guard: wire.from_main_guard,
        })
    }
}

impl Ord for SafetyError {
    fn cmp(&self, other: &Self) -> Ordering {
        // Over the fields a cache carries, and only those: the range is not one
        // of them, so ordering by it would sort a list into an order the wire
        // cannot reproduce. Records that tie here are identical on the wire.
        self.kind
            .cmp(&other.kind)
            .then_with(|| self.metadata.cmp(&other.metadata))
            .then_with(|| {
                self.parameterized_decorator
                    .cmp(&other.parameterized_decorator)
            })
            .then_with(|| self.from_main_guard.cmp(&other.from_main_guard))
    }
}

impl PartialOrd for SafetyError {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SafetyError {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for SafetyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_error_kind() {
        let out = &ErrorKind::UnsafeFunctionCall.error_string();
        assert_eq!(out, "unsafe-function-call");
    }

    /// The order is over the fields a cache carries, and position is not one of
    /// them: two errors that differ only in where they occur encode to the same
    /// bytes, so an order that separated them could not be reproduced.
    #[test]
    fn test_safety_error_ordering_is_over_cached_fields_only() {
        use std::cmp::Ordering;

        let range1 = TextRange::new(0u32.into(), 5u32.into());
        let range2 = TextRange::new(10u32.into(), 15u32.into());

        let err = SafetyError::new(ErrorKind::UnsafeFunctionCall, "foo".to_string(), range1);
        let elsewhere = SafetyError::new(ErrorKind::UnsafeFunctionCall, "foo".to_string(), range2);

        assert_eq!(err, elsewhere, "position is not part of the identity");
        assert_eq!(err.partial_cmp(&elsewhere), Some(Ordering::Equal));

        let other_kind = SafetyError::new(ErrorKind::ExecCall, "foo".to_string(), range1);
        assert_ne!(err, other_kind, "the kind is");
        assert_ne!(
            err.partial_cmp(&other_kind),
            Some(Ordering::Equal),
            "and it orders, so the encoder has a total order over cached fields",
        );
    }

    #[test]
    fn test_error_kind_for_marks_eager_kinds() {
        for kind in [
            EffectKind::CustomFinalizer,
            EffectKind::ExecCall,
            EffectKind::SysModulesAccess,
            EffectKind::SubclassesAccess,
            EffectKind::BuiltinsImportOverride,
        ] {
            assert!(
                SafetyError::error_kind_for(kind)
                    .is_some_and(|k| k.requires_eager_loading_imports()),
                "{kind:?} should map to an eager-loading error"
            );
        }
        assert!(SafetyError::error_kind_for(EffectKind::FunctionCall).is_none());
        assert!(SafetyError::error_kind_for(EffectKind::Mutation).is_none());
    }

    #[test]
    fn test_error_metadata_serialize() {
        let metadata: ErrorMetadata = "test_metadata".parse().unwrap();
        let json = serde_json::to_string(&metadata).unwrap();
        assert_eq!(json, "\"test_metadata\"");
    }

    #[test]
    fn test_safety_error_from_unsafe_call_variants() {
        use pyrefly_python::module_name::ModuleName;

        let range = TextRange::default();

        let method_eff = Effect::new(
            EffectKind::MethodCall,
            ModuleName::from_str("obj.method"),
            range,
        );
        let err = SafetyError::from_unsafe_call(&method_eff).unwrap();
        assert_eq!(err.kind, ErrorKind::UnsafeMethodCall);

        let attr_eff = Effect::new(
            EffectKind::ImportedTypeAttr,
            ModuleName::from_str("cls.attr"),
            range,
        );
        let err = SafetyError::from_unsafe_call(&attr_eff).unwrap();
        assert_eq!(err.kind, ErrorKind::UnsafeMethodCall);

        let dec_eff = Effect::new(
            EffectKind::DecoratorCall,
            ModuleName::from_str("deco"),
            range,
        );
        let err = SafetyError::from_unsafe_call(&dec_eff).unwrap();
        assert_eq!(err.kind, ErrorKind::UnsafeDecoratorCall);

        let imp_dec_eff = Effect::new(
            EffectKind::ImportedDecoratorCall,
            ModuleName::from_str("deco"),
            range,
        );
        let err = SafetyError::from_unsafe_call(&imp_dec_eff).unwrap();
        assert_eq!(err.kind, ErrorKind::UnsafeDecoratorCall);

        let raise_eff = Effect::new(EffectKind::Raise, ModuleName::from_str("err"), range);
        assert!(SafetyError::from_unsafe_call(&raise_eff).is_err());
    }
}

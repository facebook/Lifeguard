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
}

impl ErrorKind {
    // Keep in sync with EffectKind::requires_eager_loading_imports()
    pub fn requires_eager_loading_imports(&self) -> bool {
        matches!(
            self,
            Self::CustomFinalizer | Self::ExecCall | Self::SysModulesAccess
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

#[derive(Debug, Clone, Copy)]
pub struct SafetyError {
    pub kind: ErrorKind,
    pub metadata: ErrorMetadata,
    pub range: TextRange,
    /// True when this error is from a parameterized decorator (`@deco(...)`),
    /// whose returned wrapper also runs at decoration time.
    pub parameterized_decorator: bool,
}

impl SafetyError {
    pub fn new(kind: ErrorKind, metadata: String, range: TextRange) -> Self {
        Self {
            kind,
            metadata: metadata.parse().unwrap(),
            range,
            parameterized_decorator: false,
        }
    }

    pub fn new_from_effect(kind: ErrorKind, eff: &Effect) -> Self {
        Self {
            kind,
            metadata: eff.name.as_str().parse().unwrap(),
            range: eff.range,
            parameterized_decorator: is_parameterized_decorator_effect(eff),
        }
    }

    // Some effects can be converted directly into safety errors.
    pub fn from_effect(eff: &Effect) -> Option<Self> {
        match eff.kind {
            EffectKind::ProhibitedFunctionCall => Some(ErrorKind::ProhibitedCall),
            EffectKind::UnknownFunctionCall => Some(ErrorKind::UnknownFunctionCall),
            EffectKind::Raise => Some(ErrorKind::UnhandledException),
            EffectKind::CustomFinalizer => Some(ErrorKind::CustomFinalizer),
            EffectKind::ExecCall => Some(ErrorKind::ExecCall),
            EffectKind::SysModulesAccess => Some(ErrorKind::SysModulesAccess),
            EffectKind::UnknownDecoratorCall => Some(ErrorKind::UnknownDecoratorCall),
            EffectKind::UnknownEffects => Some(ErrorKind::UnknownEffects),
            EffectKind::UnknownObject => Some(ErrorKind::UnknownObject),
            EffectKind::TooManyArgs => Some(ErrorKind::TooManyArgs),
            _ => None,
        }
        .map(|kind| Self::new_from_effect(kind, eff))
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
            | EffectKind::ImportedTypeAttr => ErrorKind::UnsafeMethodCall,
            _ => return Err(anyhow!("Unexpected call effect {:?}", eff)),
        };
        Ok(Self::new_from_effect(kind, eff))
    }
}

/// Whether `eff` is a decorator applied as a call (`@deco(...)`) rather than a
/// bare `@deco`. Only the call form runs a returned wrapper at decoration time,
/// so its nested functions must also be verified safe.
fn is_parameterized_decorator_effect(eff: &Effect) -> bool {
    matches!(
        eff.kind,
        EffectKind::DecoratorCall | EffectKind::ImportedDecoratorCall
    ) && matches!(eff.data, EffectData::Call(_))
}

impl Ord for SafetyError {
    fn cmp(&self, other: &Self) -> Ordering {
        // Order errors by file location first, then by what kind of error they are.
        self.range
            .start()
            .cmp(&other.range.start())
            .then_with(|| self.range.end().cmp(&other.range.end()))
            .then_with(|| self.kind.cmp(&other.kind))
            .then_with(|| self.metadata.cmp(&other.metadata))
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

    #[test]
    fn test_safety_error_ordering_and_equality() {
        use std::cmp::Ordering;

        let range1 = TextRange::new(0u32.into(), 5u32.into());
        let range2 = TextRange::new(10u32.into(), 15u32.into());

        let err1 = SafetyError::new(ErrorKind::UnsafeFunctionCall, "foo".to_string(), range1);
        let err2 = SafetyError::new(ErrorKind::UnsafeFunctionCall, "foo".to_string(), range2);
        let err3 = SafetyError::new(ErrorKind::UnsafeFunctionCall, "foo".to_string(), range1);

        assert_eq!(err1, err3);
        assert_eq!(err1.partial_cmp(&err3), Some(Ordering::Equal));

        assert!(err1 < err2);
        assert_eq!(err1.partial_cmp(&err2), Some(Ordering::Less));

        let err4 = SafetyError::new(ErrorKind::ExecCall, "foo".to_string(), range1);
        assert_ne!(
            err1, err4,
            "different ErrorKind at same range should differ"
        );
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

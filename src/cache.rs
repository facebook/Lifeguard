/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Cache artifacts and the map-reduce pipeline that resolves them.
//!
//! The implementation is split by responsibility:
//!
//! - [`artifact`] owns the serialized cache data and construction from one
//!   library's analysis.
//! - [`bundled_stubs`] reconstructs graph-only stub modules omitted from
//!   per-library artifacts.
//! - [`merge`] coalesces records from multiple libraries and propagates facts
//!   that are independent of final safety resolution.
//! - [`reduce`] owns the unresolved-to-resolved phase transition.

mod artifact;
mod bundled_stubs;
mod merge;
mod reduce;

pub(crate) use crate::cache::artifact::CONSTRUCTOR_METHODS;
pub use crate::cache::artifact::CachedError;
pub use crate::cache::artifact::CachedExports;
pub use crate::cache::artifact::CachedModule;
pub use crate::cache::artifact::CachedModuleSafety;
pub use crate::cache::artifact::CachedReExport;
pub use crate::cache::artifact::CachedSafety;
pub use crate::cache::artifact::ConstructorCallees;
pub use crate::cache::artifact::LibraryCache;
pub(crate) use crate::cache::artifact::constructor_mask_bits;
pub use crate::cache::artifact::own_constructor_bit;
pub use crate::cache::merge::dedupe_implicit_imports;
pub use crate::cache::reduce::MergedClassFacts;
pub use crate::cache::reduce::ReduceWorkspace;
pub use crate::cache::reduce::ResolvedCache;
#[doc(hidden)]
pub use crate::safety_resolver::is_call_verified_safe;

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Fixed-seed hashing for the analyzer's many small maps and sets.
//!
//! `ahash::RandomState` (the default behind `ahash::AHashMap`/`AHashSet`)
//! generates a fresh random seed on every map/set creation via
//! `gen_hasher_seed`. Profiling showed that dominated CPU because the analyzer
//! builds millions of small maps. As a batch tool it gains nothing from
//! randomized (DoS-resistant) hashing, so we use a fixed compile-time seed.

use std::collections::hash_map::Entry;
use std::hash::BuildHasherDefault;

use ahash::AHasher;
// Re-exported so call sites get `new()` / `with_capacity()` for the fixed-seed
// maps below (these methods come from ahash's ext traits, not inherent impls).
pub use ahash::HashMapExt;
pub use ahash::HashSetExt;

/// Deterministic, fixed-seed hasher state.
pub type FixedState = BuildHasherDefault<AHasher>;

/// Drop-in replacement for `ahash::AHashMap` using a fixed seed.
///
/// Backed by `std::collections::HashMap` (not ahash's newtype) so it keeps the
/// generic `Default`/`serde` impls; `new()`/`with_capacity()` come from the
/// re-exported [`HashMapExt`] trait.
pub type AHashMap<K, V> = std::collections::HashMap<K, V, FixedState>;

/// Drop-in replacement for `ahash::AHashSet` using a fixed seed.
pub type AHashSet<K> = std::collections::HashSet<K, FixedState>;

/// Union two sets by extending the larger allocation with the smaller one to minimize rehashing.
pub fn union_larger<T>(a: AHashSet<T>, b: AHashSet<T>) -> AHashSet<T>
where
    T: Eq + std::hash::Hash,
{
    let (mut large, small) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    large.extend(small);
    large
}

/// Merge a nested map into another, moving each inner collection wholesale when
/// its key is new. Nesting a map is only worth it if merges stay proportional to
/// the outer key count, so the miss path must not rehash the inner entries.
pub fn extend_nested<K, V>(dst: &mut AHashMap<K, V>, src: AHashMap<K, V>)
where
    K: Eq + std::hash::Hash,
    V: IntoIterator + Extend<<V as IntoIterator>::Item>,
{
    for (key, inner) in src {
        match dst.entry(key) {
            Entry::Occupied(mut slot) => slot.get_mut().extend(inner),
            Entry::Vacant(slot) => {
                slot.insert(inner);
            }
        }
    }
}

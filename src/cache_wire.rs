/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Indexed wire format for library caches.
//!
//! The encoder is *canonical*: equal in-memory caches must produce byte-identical
//! files. Buck keys downstream reduce actions on artifact content, so unstable bytes
//! would defeat cache reuse for an unchanged library.
//!
//! Canonicality is the encoder's responsibility, not the caller's: every
//! order-insensitive collection is sorted here rather than assumed sorted.
//! Order-*sensitive* sequences are the exception and are encoded verbatim — see
//! `CachedModule::mutation_candidates` and `class_bases`, both noted below.

use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use ruff_python_ast::name::Name;
use ruff_text_size::TextRange;
use serde::Deserialize;
use serde::Serialize;

use crate::cache::CachedExports;
use crate::cache::CachedModule;
use crate::cache::CachedModuleSafety;
use crate::cache::CachedReExport;
use crate::cache::CachedReturnType;
use crate::cache::CachedSafety;
use crate::cache::ConstructorCallees;
use crate::cache::LibraryCache;
use crate::cache::MainGuardFacts;
use crate::effects::ImportedArgs;
use crate::errors::SafetyError;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashSetExt;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::MutatedParam;
use crate::module_safety::MutationCandidate;
use crate::module_safety::MutationCandidateSite;
use crate::module_safety::ParamPosition;
use crate::module_safety::PropertyCandidate;

type NameId = u32;

/// Write buffer for the cache: caches are O(hundreds of MB), so a large buffer
/// minimizes write syscalls (matches the JSON writers in `commands`).
const WRITE_BUFFER_CAPACITY: usize = 1 << 20;

#[derive(Serialize, Deserialize)]
struct WireHeader {
    names: Vec<ModuleName>,
    exports: Vec<WireReExport>,
    return_types: Vec<(NameId, NameId)>,
    class_bases: Vec<(NameId, Vec<NameId>)>,
    /// Class id, its metaclass id when a metaclass bit is set, and the callee
    /// mask. The callee FQNs themselves are reconstructible from these, so they
    /// stay out of the name table entirely.
    constructor_callees: Vec<(NameId, Option<NameId>, u8, Vec<NameId>)>,
    class_properties: Vec<(NameId, Vec<String>)>,
}

#[derive(Serialize, Deserialize)]
struct WireModule {
    name: NameId,
    safety: WireSafety,
    imports: Vec<NameId>,
    missing_imports: Vec<NameId>,
    ambiguous_imports: Vec<NameId>,
    side_effect_imports: Vec<NameId>,
    main_guard_imports: Vec<NameId>,
    function_safety: Vec<(String, WireFunctionSafetyInfo)>,
    mutation_candidates: Vec<WireMutationCandidate>,
    property_candidates: Vec<WirePropertyCandidate>,
}

#[derive(Serialize, Deserialize)]
enum WireSafety {
    Ok {
        errors: Vec<SafetyError>,
        force_imports_eager_overrides: Vec<SafetyError>,
        implicit_imports: Vec<NameId>,
    },
    AnalysisError {
        message: String,
    },
}

#[derive(Serialize, Deserialize)]
struct WireFunctionSafetyInfo {
    verdict: FunctionSafety,
    missing_dep_callees: Vec<NameId>,
    mutated_params: Vec<WireMutatedParam>,
}

#[derive(Serialize, Deserialize)]
struct WireMutatedParam {
    name: NameId,
    position: ParamPosition,
}

#[derive(Serialize, Deserialize)]
struct WirePropertyCandidate {
    attribute: NameId,
    from_main_guard: bool,
}

#[derive(Serialize, Deserialize)]
struct WireMutationCandidate {
    callee: NameId,
    site: WireMutationCandidateSite,
    arg_offset: usize,
    imported_args: WireImportedArgs,
    from_main_guard: bool,
}

#[derive(Serialize, Deserialize)]
enum WireMutationCandidateSite {
    ModuleScope { call: NameId },
    Function { name: NameId },
}

#[derive(Serialize, Deserialize)]
struct WireImportedArgs {
    unsafe_arg_indices: u64,
    unsafe_keyword_names: Vec<NameId>,
    has_unsafe_kwargs_expansion: bool,
    unsafe_args_expansion_min: Option<usize>,
}

#[derive(Serialize, Deserialize)]
struct WireReExport {
    exported_module: NameId,
    exported_attr: String,
    imported_module: NameId,
    imported_attr: String,
}

// Ids are lexicographic ranks over the list of names, so sorting
// a `Vec<NameId>` sorts by module name.
struct NameTable {
    names: Vec<ModuleName>,
    ids: AHashMap<ModuleName, NameId>,
}

impl NameTable {
    fn build(cache: &LibraryCache) -> Result<Self> {
        let mut unique = AHashSet::with_capacity(cache.modules.len());
        for module in &cache.modules {
            collect_module_names(module, &mut unique);
        }
        for re_export in &cache.exports.re_exports {
            unique.insert(re_export.exported_module);
            unique.insert(re_export.imported_module);
        }
        for return_type in &cache.exports.return_types {
            unique.insert(return_type.function);
            unique.insert(return_type.class);
        }
        for (class, bases) in &cache.class_bases {
            unique.insert(*class);
            unique.extend(bases.iter().copied());
        }
        for (class, recorded) in &cache.constructor_callees {
            unique.insert(*class);
            unique.extend(recorded.metaclass);
            unique.extend(recorded.extra.iter().copied());
        }
        for (class, _) in &cache.class_properties {
            unique.insert(*class);
        }

        ensure!(
            unique.len() <= NameId::MAX as usize,
            "cache contains too many distinct module names"
        );
        let mut names: Vec<ModuleName> = unique.into_iter().collect();
        names.par_sort_unstable();
        let ids = names
            .iter()
            .enumerate()
            .map(|(id, name)| (*name, id as NameId))
            .collect();
        Ok(Self { names, ids })
    }

    fn id(&self, name: ModuleName) -> NameId {
        *self
            .ids
            .get(&name)
            .expect("all cached module names should be in the wire name table")
    }

    /// Encode an order-insensitive name collection as ids in canonical order.
    /// Duplicates are preserved, so a multiset encodes faithfully.
    fn encode_sorted<'a>(&self, names: impl IntoIterator<Item = &'a ModuleName>) -> Vec<NameId> {
        let mut ids: Vec<NameId> = names.into_iter().map(|name| self.id(*name)).collect();
        ids.sort_unstable();
        ids
    }
}

/// Errors in canonical order. `SafetyError`'s `Ord` is a total order over all of
/// its fields, so this does not depend on how errors were accumulated.
fn sorted_errors(errors: &[SafetyError]) -> Vec<SafetyError> {
    let mut errors = errors.to_vec();
    errors.sort_unstable();
    errors
}

fn collect_module_names(module: &CachedModule, names: &mut AHashSet<ModuleName>) {
    names.insert(module.name);
    names.extend(module.imports.iter().copied());
    names.extend(module.missing_imports.iter().copied());
    names.extend(module.ambiguous_imports.iter().copied());
    names.extend(module.side_effect_imports.iter().copied());
    // A subset of `imports` in practice, but collected explicitly so the table
    // does not depend on that holding.
    names.extend(module.main_guard.imports().iter().copied());
    if let CachedSafety::Ok(safety) = &module.safety {
        names.extend(safety.implicit_imports.iter().copied());
    }
    for info in module.function_safety.values() {
        names.extend(info.missing_dep_callees.iter().copied());
        names.extend(info.mutated_params.iter().map(|param| param.name));
    }
    for candidate in &module.mutation_candidates {
        names.insert(candidate.callee);
        match candidate.site {
            MutationCandidateSite::ModuleScope { call } => {
                names.insert(call);
            }
            MutationCandidateSite::Function { name } => {
                names.insert(name);
            }
        }
        names.extend(candidate.imported_args.unsafe_keyword_names.iter().copied());
    }
    names.extend(
        module
            .property_candidates
            .iter()
            .map(|candidate| candidate.attribute),
    );
}

pub(crate) fn write(cache: &LibraryCache, path: &Path) -> Result<()> {
    let table = NameTable::build(cache)?;

    // Modules are written in name order, with the encoded blob breaking ties.
    //
    // Name alone would not be a total order: nothing in `LibraryCache`
    // prevents two records naming one logical module.
    // NOTE: `merge_dep_caches` coalesces such records, but canonicality here
    // should not rest on a caller's invariant.
    let mut module_blobs: Vec<(NameId, Vec<u8>)> = cache
        .modules
        .par_iter()
        .map(|module| {
            let blob = postcard::to_allocvec(&WireModule::encode(module, &table))?;
            Ok((table.id(module.name), blob))
        })
        .collect::<Result<_>>()?;
    module_blobs.par_sort_unstable();

    let mut exports: Vec<WireReExport> = cache
        .exports
        .re_exports
        .iter()
        .map(|re_export| WireReExport::encode(re_export, &table.ids))
        .collect();
    exports.sort_unstable_by(|a, b| a.sort_key().cmp(&b.sort_key()));

    let mut return_types: Vec<(NameId, NameId)> = cache
        .exports
        .return_types
        .iter()
        .map(|rt| (table.id(rt.function), table.id(rt.class)))
        .collect();
    return_types.sort_unstable();

    // A class's base list is declaration order, which C3 depends on, so only the
    // outer sequence is sorted; whole tuples keep the order total across libraries.
    let mut class_bases: Vec<(NameId, Vec<NameId>)> = cache
        .class_bases
        .iter()
        .map(|(class, bases)| {
            (
                table.id(*class),
                bases.iter().map(|base| table.id(*base)).collect(),
            )
        })
        .collect();
    class_bases.sort_unstable();

    // The map phase collects these in parallel, so the sequence arrives in
    // nondeterministic order. `extra` is an order-insensitive callee set, so it
    // is sorted too rather than left to whichever library contributed first.
    let mut constructor_callees: Vec<(NameId, Option<NameId>, u8, Vec<NameId>)> = cache
        .constructor_callees
        .iter()
        .map(|(class, recorded)| {
            let mut extra: Vec<NameId> = recorded.extra.iter().map(|c| table.id(*c)).collect();
            extra.sort_unstable();
            (
                table.id(*class),
                recorded.metaclass.map(|metaclass| table.id(metaclass)),
                recorded.mask,
                extra,
            )
        })
        .collect();
    constructor_callees.sort_unstable();

    // Property names within a class are order-insensitive, so both levels sort.
    let mut class_properties: Vec<(NameId, Vec<String>)> = cache
        .class_properties
        .iter()
        .map(|(class, properties)| {
            let mut properties = properties.clone();
            properties.sort_unstable();
            (table.id(*class), properties)
        })
        .collect();
    class_properties.sort_unstable();

    let header = WireHeader {
        names: table.names,
        exports,
        return_types,
        class_bases,
        constructor_callees,
        class_properties,
    };
    let header_bytes = postcard::to_allocvec(&header)?;

    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_CAPACITY, file);
    write_len(&mut writer, header_bytes.len())?;
    writer.write_all(&header_bytes)?;
    write_len(&mut writer, module_blobs.len())?;
    for (_, blob) in module_blobs {
        write_len(&mut writer, blob.len())?;
        writer.write_all(&blob)?;
    }
    writer.flush()?;
    Ok(())
}

pub(crate) fn read(path: &Path) -> Result<LibraryCache> {
    let bytes = std::fs::read(path)?;
    let mut offset = 0;
    let header_len = read_len(&bytes, &mut offset)?;
    let header_end = offset
        .checked_add(header_len)
        .context("cache header length overflow")?;
    ensure!(
        header_end <= bytes.len(),
        "truncated Lifeguard cache header"
    );
    let header: WireHeader = postcard::from_bytes(&bytes[offset..header_end])?;
    offset = header_end;

    let module_count = read_len(&bytes, &mut offset)?;
    // Each module contributes at least an 8-byte length prefix, so a count larger
    // than the remaining bytes / 8 is corrupt. Reject it before allocating rather
    // than attempting a huge `Vec::with_capacity`.
    ensure!(
        module_count <= (bytes.len() - offset) / 8,
        "cache module count {module_count} exceeds remaining bytes"
    );
    let mut blobs = Vec::with_capacity(module_count);
    for _ in 0..module_count {
        let blob_len = read_len(&bytes, &mut offset)?;
        let blob_end = offset
            .checked_add(blob_len)
            .context("cache module length overflow")?;
        ensure!(blob_end <= bytes.len(), "truncated Lifeguard cache module");
        blobs.push(&bytes[offset..blob_end]);
        offset = blob_end;
    }
    ensure!(offset == bytes.len(), "trailing data in Lifeguard cache");

    let modules = blobs
        .par_iter()
        .map(|blob| {
            let wire: WireModule = postcard::from_bytes(blob)?;
            wire.decode(&header.names)
        })
        .collect::<Result<Vec<_>>>()?;
    let exports = CachedExports {
        re_exports: header
            .exports
            .into_iter()
            .map(|re_export| re_export.decode(&header.names))
            .collect::<Result<_>>()?,
        return_types: header
            .return_types
            .into_iter()
            .map(|(function, class)| {
                Ok(CachedReturnType {
                    function: decode_name(&header.names, function)?,
                    class: decode_name(&header.names, class)?,
                })
            })
            .collect::<Result<_>>()?,
    };
    let class_bases = header
        .class_bases
        .into_iter()
        .map(|(class, bases)| {
            Ok((
                decode_name(&header.names, class)?,
                decode_names(&header.names, bases)?,
            ))
        })
        .collect::<Result<_>>()?;
    let constructor_callees = header
        .constructor_callees
        .into_iter()
        .map(|(class, metaclass, mask, extra)| {
            Ok((
                decode_name(&header.names, class)?,
                ConstructorCallees {
                    metaclass: metaclass
                        .map(|id| decode_name(&header.names, id))
                        .transpose()?,
                    mask,
                    extra: decode_names(&header.names, extra)?,
                },
            ))
        })
        .collect::<Result<_>>()?;
    let class_properties = header
        .class_properties
        .into_iter()
        .map(|(class, properties)| Ok((decode_name(&header.names, class)?, properties)))
        .collect::<Result<_>>()?;
    Ok(LibraryCache {
        modules,
        exports,
        class_bases,
        class_properties,
        constructor_callees,
        ..Default::default()
    })
}

fn write_len(writer: &mut impl Write, len: usize) -> Result<()> {
    let len = u64::try_from(len).context("cache length does not fit in u64")?;
    writer.write_all(&len.to_le_bytes())?;
    Ok(())
}

fn read_len(bytes: &[u8], offset: &mut usize) -> Result<usize> {
    let end = offset.checked_add(8).context("cache offset overflow")?;
    let raw: [u8; 8] = bytes
        .get(*offset..end)
        .context("truncated Lifeguard cache length")?
        .try_into()
        .expect("an eight-byte slice should convert to an eight-byte array");
    *offset = end;
    usize::try_from(u64::from_le_bytes(raw)).context("cache length does not fit in usize")
}

fn decode_name(names: &[ModuleName], id: NameId) -> Result<ModuleName> {
    names
        .get(id as usize)
        .copied()
        .with_context(|| format!("cache module-name id {id} is out of bounds"))
}

fn decode_names(names: &[ModuleName], ids: Vec<NameId>) -> Result<Vec<ModuleName>> {
    ids.into_iter().map(|id| decode_name(names, id)).collect()
}

fn decode_name_set(names: &[ModuleName], ids: Vec<NameId>) -> Result<AHashSet<ModuleName>> {
    ids.into_iter().map(|id| decode_name(names, id)).collect()
}

impl WireModule {
    fn encode(module: &CachedModule, table: &NameTable) -> Self {
        let mut function_safety: Vec<(&String, &FunctionSafetyInfo)> =
            module.function_safety.iter().collect();
        function_safety.sort_unstable_by_key(|(name, _)| *name);

        let mut property_candidates: Vec<&PropertyCandidate> =
            module.property_candidates.iter().collect();
        property_candidates.sort_unstable();

        Self {
            name: table.id(module.name),
            safety: WireSafety::encode(&module.safety, table),
            imports: table.encode_sorted(&module.imports),
            missing_imports: table.encode_sorted(&module.missing_imports),
            ambiguous_imports: table.encode_sorted(&module.ambiguous_imports),
            side_effect_imports: table.encode_sorted(&module.side_effect_imports),
            main_guard_imports: table.encode_sorted(module.main_guard.imports()),
            function_safety: function_safety
                .into_iter()
                .map(|(name, info)| (name.clone(), WireFunctionSafetyInfo::encode(info, table)))
                .collect(),
            // Not sorted: `apply_mutation_candidates` observes verdict writes
            // from earlier candidates, so this sequence is order-sensitive.
            mutation_candidates: module
                .mutation_candidates
                .iter()
                .map(|candidate| WireMutationCandidate::encode(candidate, table))
                .collect(),
            // Sorted: unlike mutation candidates, property candidates are resolved
            // independently, so no order carries meaning.
            property_candidates: property_candidates
                .into_iter()
                .map(|candidate| WirePropertyCandidate::encode(candidate, table))
                .collect(),
        }
    }

    fn decode(self, names: &[ModuleName]) -> Result<CachedModule> {
        Ok(CachedModule {
            name: decode_name(names, self.name)?,
            safety: self.safety.decode(names)?,
            imports: decode_name_set(names, self.imports)?,
            missing_imports: decode_name_set(names, self.missing_imports)?,
            ambiguous_imports: decode_name_set(names, self.ambiguous_imports)?,
            side_effect_imports: decode_name_set(names, self.side_effect_imports)?,
            main_guard: MainGuardFacts::new(decode_name_set(names, self.main_guard_imports)?),
            function_safety: self
                .function_safety
                .into_iter()
                .map(|(name, info)| Ok((name, info.decode(names)?)))
                .collect::<Result<_>>()?,
            mutation_candidates: self
                .mutation_candidates
                .into_iter()
                .map(|candidate| candidate.decode(names))
                .collect::<Result<_>>()?,
            property_candidates: self
                .property_candidates
                .into_iter()
                .map(|candidate| candidate.decode(names))
                .collect::<Result<_>>()?,
        })
    }
}

impl WirePropertyCandidate {
    fn encode(candidate: &PropertyCandidate, table: &NameTable) -> Self {
        Self {
            attribute: table.id(candidate.attribute),
            from_main_guard: candidate.from_main_guard,
        }
    }

    fn decode(self, names: &[ModuleName]) -> Result<PropertyCandidate> {
        Ok(PropertyCandidate {
            attribute: decode_name(names, self.attribute)?,
            // Not carried: a cached offset would tie the bytes to where in the
            // file the access sits.
            range: TextRange::default(),
            from_main_guard: self.from_main_guard,
        })
    }
}

impl WireSafety {
    fn encode(safety: &CachedSafety, table: &NameTable) -> Self {
        match safety {
            CachedSafety::Ok(safety) => Self::Ok {
                // Errors are consumed as multisets (deduped for clearing, counted
                // for reporting), so sorting is canonicalization. Duplicates are
                // kept: `--explain` reports a repeated error's count.
                errors: sorted_errors(&safety.errors),
                force_imports_eager_overrides: sorted_errors(&safety.force_imports_eager_overrides),
                implicit_imports: table.encode_sorted(&safety.implicit_imports),
            },
            CachedSafety::AnalysisError { message } => Self::AnalysisError {
                message: message.clone(),
            },
        }
    }

    fn decode(self, names: &[ModuleName]) -> Result<CachedSafety> {
        Ok(match self {
            Self::Ok {
                errors,
                force_imports_eager_overrides,
                implicit_imports,
            } => CachedSafety::Ok(CachedModuleSafety {
                errors,
                force_imports_eager_overrides,
                implicit_imports: decode_names(names, implicit_imports)?,
            }),
            Self::AnalysisError { message } => CachedSafety::AnalysisError { message },
        })
    }
}

impl WireFunctionSafetyInfo {
    fn encode(info: &FunctionSafetyInfo, table: &NameTable) -> Self {
        let mut mutated_params: Vec<WireMutatedParam> = info
            .mutated_params
            .iter()
            .map(|param| WireMutatedParam {
                name: table.id(param.name),
                position: param.position,
            })
            .collect();
        mutated_params.sort_unstable_by_key(|param| param.name);

        Self {
            verdict: info.verdict,
            missing_dep_callees: table.encode_sorted(&info.missing_dep_callees),
            mutated_params,
        }
    }

    fn decode(self, names: &[ModuleName]) -> Result<FunctionSafetyInfo> {
        Ok(FunctionSafetyInfo {
            verdict: self.verdict,
            missing_dep_callees: decode_name_set(names, self.missing_dep_callees)?,
            mutated_params: self
                .mutated_params
                .into_iter()
                .map(|param| {
                    Ok(MutatedParam {
                        name: decode_name(names, param.name)?,
                        position: param.position,
                    })
                })
                .collect::<Result<_>>()?,
        })
    }
}

impl WireMutationCandidate {
    fn encode(candidate: &MutationCandidate, table: &NameTable) -> Self {
        let site = match candidate.site {
            MutationCandidateSite::ModuleScope { call } => WireMutationCandidateSite::ModuleScope {
                call: table.id(call),
            },
            MutationCandidateSite::Function { name } => WireMutationCandidateSite::Function {
                name: table.id(name),
            },
        };
        Self {
            callee: table.id(candidate.callee),
            site,
            arg_offset: candidate.arg_offset,
            from_main_guard: candidate.from_main_guard,
            imported_args: WireImportedArgs {
                unsafe_arg_indices: candidate.imported_args.unsafe_arg_indices,
                // Read only through `has_unsafe_keyword`, so this is a set.
                unsafe_keyword_names: table
                    .encode_sorted(&candidate.imported_args.unsafe_keyword_names),
                has_unsafe_kwargs_expansion: candidate.imported_args.has_unsafe_kwargs_expansion,
                unsafe_args_expansion_min: candidate.imported_args.unsafe_args_expansion_min,
            },
        }
    }

    fn decode(self, names: &[ModuleName]) -> Result<MutationCandidate> {
        let site = match self.site {
            WireMutationCandidateSite::ModuleScope { call } => MutationCandidateSite::ModuleScope {
                call: decode_name(names, call)?,
            },
            WireMutationCandidateSite::Function { name } => MutationCandidateSite::Function {
                name: decode_name(names, name)?,
            },
        };
        Ok(MutationCandidate {
            callee: decode_name(names, self.callee)?,
            site,
            arg_offset: self.arg_offset,
            // Not carried: a cached offset would tie the bytes to where in the
            // file the call sits.
            range: TextRange::default(),
            from_main_guard: self.from_main_guard,
            imported_args: ImportedArgs {
                unsafe_arg_indices: self.imported_args.unsafe_arg_indices,
                unsafe_keyword_names: decode_names(names, self.imported_args.unsafe_keyword_names)?,
                has_unsafe_kwargs_expansion: self.imported_args.has_unsafe_kwargs_expansion,
                unsafe_args_expansion_min: self.imported_args.unsafe_args_expansion_min,
            },
        })
    }
}

impl WireReExport {
    /// Total order over every field. Sorting by the exported `(module, attr)`
    /// alone would leave the order of two records that share it unspecified.
    fn sort_key(&self) -> (NameId, &str, NameId, &str) {
        (
            self.exported_module,
            &self.exported_attr,
            self.imported_module,
            &self.imported_attr,
        )
    }

    fn encode(re_export: &CachedReExport, ids: &AHashMap<ModuleName, NameId>) -> Self {
        let id = |name| {
            *ids.get(&name)
                .expect("all re-export module names should be in the wire name table")
        };
        Self {
            exported_module: id(re_export.exported_module),
            exported_attr: re_export.exported_attr.to_string(),
            imported_module: id(re_export.imported_module),
            imported_attr: re_export.imported_attr.to_string(),
        }
    }

    fn decode(self, names: &[ModuleName]) -> Result<CachedReExport> {
        Ok(CachedReExport {
            exported_module: decode_name(names, self.exported_module)?,
            exported_attr: Name::new(self.exported_attr),
            imported_module: decode_name(names, self.imported_module)?,
            imported_attr: Name::new(self.imported_attr),
        })
    }
}

#[cfg(test)]
mod tests {
    use rayon::ThreadPoolBuilder;
    use tempfile::TempDir;

    use super::*;
    use crate::cache::CachedExports;
    use crate::cache::CachedModuleSafety;
    use crate::errors::ErrorKind;
    use crate::hasher::HashMapExt;

    fn mn(name: &str) -> ModuleName {
        ModuleName::from_str(name)
    }

    /// Reverse a sequence when `reversed`, leaving its contents unchanged.
    fn arrange<T>(reversed: bool, mut items: Vec<T>) -> Vec<T> {
        if reversed {
            items.reverse();
        }
        items
    }

    fn error(metadata: &str) -> SafetyError {
        SafetyError::new(
            ErrorKind::UnsafeFunctionCall,
            metadata.to_owned(),
            TextRange::default(),
        )
    }

    fn candidate(callee: &str, caller: &str) -> MutationCandidate {
        MutationCandidate {
            callee: mn(callee),
            site: MutationCandidateSite::Function { name: mn(caller) },
            arg_offset: 0,
            range: TextRange::default(),
            from_main_guard: false,
            imported_args: ImportedArgs {
                unsafe_arg_indices: 1,
                unsafe_keyword_names: vec![mn("zeta_kw"), mn("alpha_kw")],
                has_unsafe_kwargs_expansion: false,
                unsafe_args_expansion_min: None,
            },
        }
    }

    /// A cache exercising every collection the encoder touches, with names chosen
    /// so that insertion order and lexicographic order disagree.
    ///
    /// `reversed` flips each in-memory sequence. The logical content is identical
    /// either way, which is what lets the permutation tests below distinguish a
    /// canonical encoder from one that mirrors its input's order.
    fn property(attribute: &str) -> PropertyCandidate {
        PropertyCandidate {
            attribute: mn(attribute),
            range: TextRange::default(),
            from_main_guard: false,
        }
    }

    fn fixture_cache(reversed: bool) -> LibraryCache {
        let mut function_safety = AHashMap::new();
        let mut zeta_func = FunctionSafetyInfo::new(FunctionSafety::UnsafeMissingDep);
        zeta_func.missing_dep_callees = [mn("dep.zeta"), mn("dep.alpha")].into_iter().collect();
        zeta_func.mutated_params = arrange(
            reversed,
            vec![
                MutatedParam {
                    name: mn("zeta_param"),
                    position: ParamPosition::Positional(1),
                },
                MutatedParam {
                    name: mn("alpha_param"),
                    position: ParamPosition::Positional(0),
                },
            ],
        );
        function_safety.insert("zeta_func".to_owned(), zeta_func);
        function_safety.insert(
            "alpha_func".to_owned(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        );

        let zeta = CachedModule {
            name: mn("pkg.zeta"),
            main_guard: MainGuardFacts::new(
                [mn("guard.zeta"), mn("guard.alpha")].into_iter().collect(),
            ),
            property_candidates: arrange(
                reversed,
                vec![property("pkg.Zeta.attr"), property("pkg.Alpha.attr")],
            ),
            safety: CachedSafety::Ok(CachedModuleSafety {
                errors: arrange(reversed, vec![error("zeta.call()"), error("alpha.call()")]),
                force_imports_eager_overrides: Vec::new(),
                implicit_imports: arrange(reversed, vec![mn("pkg.zeta.sub"), mn("pkg.alpha.sub")]),
            }),
            imports: [mn("pkg.zeta.dep"), mn("pkg.alpha.dep")]
                .into_iter()
                .collect(),
            missing_imports: [mn("missing.zeta"), mn("missing.alpha")]
                .into_iter()
                .collect(),
            ambiguous_imports: [mn("ambiguous.zeta"), mn("ambiguous.alpha")]
                .into_iter()
                .collect(),
            side_effect_imports: [mn("side.zeta"), mn("side.alpha")].into_iter().collect(),
            function_safety,
            // Not subject to `arrange`: this sequence is order-sensitive, so
            // permuting it is a change of content, not of arrangement. The
            // dedicated test below permutes it to assert exactly that.
            mutation_candidates: vec![
                candidate("dep.zeta", "zeta_func"),
                candidate("dep.alpha", "alpha_func"),
            ],
        };
        let alpha = CachedModule {
            name: mn("pkg.alpha"),
            main_guard: MainGuardFacts::default(),
            property_candidates: Vec::new(),
            safety: CachedSafety::AnalysisError {
                message: "parse error".to_owned(),
            },
            imports: AHashSet::new(),
            missing_imports: AHashSet::new(),
            ambiguous_imports: AHashSet::new(),
            side_effect_imports: AHashSet::new(),
            function_safety: AHashMap::new(),
            mutation_candidates: Vec::new(),
        };

        let re_export = |exported: &str, attr: &str| CachedReExport {
            exported_module: mn(exported),
            exported_attr: attr.into(),
            imported_module: mn("dep.source"),
            imported_attr: "value".into(),
        };

        LibraryCache {
            modules: arrange(reversed, vec![zeta, alpha]),
            exports: CachedExports {
                return_types: Vec::new(),
                re_exports: arrange(
                    reversed,
                    vec![
                        re_export("pkg.zeta", "zeta_attr"),
                        re_export("pkg.alpha", "alpha_attr"),
                    ],
                ),
            },
            class_bases: arrange(
                reversed,
                vec![
                    (mn("pkg.zeta.Zeta"), vec![mn("pkg.b.B"), mn("pkg.a.A")]),
                    (mn("pkg.alpha.Alpha"), vec![mn("pkg.a.A")]),
                ],
            ),
            ..Default::default()
        }
    }

    fn encoded_bytes(cache: &LibraryCache) -> Vec<u8> {
        let dir = TempDir::new().expect("temp dir should be creatable");
        let path = dir.path().join("library-cache.bin");
        write(cache, &path).expect("cache write should succeed");
        std::fs::read(&path).expect("written cache should be readable")
    }

    #[test]
    fn name_table_ids_are_lexicographic_ranks() {
        let table = NameTable::build(&fixture_cache(false)).expect("name table should build");

        assert!(
            table.names.is_sorted(),
            "name ids are assigned by lexicographic rank, so the table must be sorted",
        );
        assert!(
            table.id(mn("pkg.alpha")) < table.id(mn("pkg.zeta")),
            "id order must follow name order, since the encoder sorts ids to sort by name",
        );
    }

    #[test]
    fn encoded_module_collections_are_canonically_ordered() {
        let cache = fixture_cache(false);
        let table = NameTable::build(&cache).expect("name table should build");
        let zeta = cache
            .modules
            .iter()
            .find(|module| module.name == mn("pkg.zeta"))
            .expect("fixture should contain pkg.zeta");

        let wire = WireModule::encode(zeta, &table);

        assert!(wire.imports.is_sorted(), "imports must encode sorted");
        assert!(
            wire.missing_imports.is_sorted(),
            "missing imports must encode sorted"
        );
        assert!(
            wire.ambiguous_imports.is_sorted(),
            "ambiguous imports must encode sorted"
        );
        assert!(
            wire.side_effect_imports.is_sorted(),
            "side-effect imports must encode sorted"
        );
        assert!(
            wire.main_guard_imports.is_sorted(),
            "main-guard imports must encode sorted"
        );
        assert!(
            wire.property_candidates
                .is_sorted_by_key(|candidate| candidate.attribute),
            "property candidates must encode in attribute order",
        );
        assert!(
            wire.function_safety
                .is_sorted_by_key(|(name, _)| name.as_str()),
            "function safety must encode in local-name order",
        );
        let WireSafety::Ok {
            ref errors,
            ref implicit_imports,
            ..
        } = wire.safety
        else {
            panic!("fixture module pkg.zeta should encode as WireSafety::Ok");
        };
        assert!(errors.is_sorted(), "errors must encode sorted");
        assert!(
            implicit_imports.is_sorted(),
            "implicit imports must encode sorted"
        );

        let zeta_func = wire
            .function_safety
            .iter()
            .find(|(name, _)| name == "zeta_func")
            .map(|(_, info)| info)
            .expect("fixture should contain zeta_func");
        assert!(
            zeta_func.missing_dep_callees.is_sorted(),
            "missing-dep callees must encode sorted",
        );
        assert!(
            zeta_func
                .mutated_params
                .is_sorted_by_key(|param| param.name),
            "mutated params must encode in name order",
        );
        assert!(
            wire.mutation_candidates[0]
                .imported_args
                .unsafe_keyword_names
                .is_sorted(),
            "unsafe keyword names must encode sorted",
        );
    }

    #[test]
    fn encoding_is_invariant_under_input_permutation() {
        assert_eq!(
            encoded_bytes(&fixture_cache(false)),
            encoded_bytes(&fixture_cache(true)),
            "two orderings of the same facts must produce byte-identical caches",
        );
    }

    #[test]
    fn encoding_is_invariant_under_duplicate_module_permutation() {
        // Sorting modules by name alone would not be a total order. `LibraryCache`
        // does not prevent two records naming one logical module -- the merge
        // coalesces them, but the encoder must not depend on that -- and under an
        // unstable sort two same-named records would keep an unspecified relative
        // order, so permuting them would move bytes.
        let duplicated = |reversed: bool| {
            let module = |import: &str| CachedModule {
                name: mn("pkg.duplicated"),
                main_guard: MainGuardFacts::default(),
                property_candidates: Vec::new(),
                safety: CachedSafety::Ok(CachedModuleSafety::default()),
                imports: [mn(import)].into_iter().collect(),
                missing_imports: AHashSet::new(),
                ambiguous_imports: AHashSet::new(),
                side_effect_imports: AHashSet::new(),
                function_safety: AHashMap::new(),
                mutation_candidates: Vec::new(),
            };
            LibraryCache {
                modules: arrange(reversed, vec![module("dep.zeta"), module("dep.alpha")]),
                exports: CachedExports {
                    return_types: Vec::new(),
                    re_exports: Vec::new(),
                },
                ..Default::default()
            }
        };

        assert_eq!(
            encoded_bytes(&duplicated(false)),
            encoded_bytes(&duplicated(true)),
            "duplicate module records must encode in a total order, not input order",
        );
    }

    #[test]
    fn encoding_is_stable_across_rayon_thread_counts() {
        let cache = fixture_cache(false);
        let encode_with = |threads: usize| {
            ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("test thread pool should build")
                .install(|| encoded_bytes(&cache))
        };

        assert_eq!(
            encode_with(1),
            encode_with(8),
            "the encoder's parallel sorts and maps must not let worker count reach the bytes",
        );
    }

    #[test]
    fn encoding_preserves_order_sensitive_sequences() {
        // Both sequences below are deliberately *not* canonicalized: mutation
        // candidates are applied in order by `apply_mutation_candidates`, and a
        // class's base list is its declaration order, which C3 depends on.
        // Sorting either would silently change analysis results, so assert the
        // encoder still round-trips their order.
        let mut swapped = fixture_cache(false);
        let zeta = swapped
            .modules
            .iter_mut()
            .find(|module| module.name == mn("pkg.zeta"))
            .expect("fixture should contain pkg.zeta");
        zeta.mutation_candidates.reverse();
        assert_ne!(
            encoded_bytes(&fixture_cache(false)),
            encoded_bytes(&swapped),
            "mutation candidate order is semantic and must survive encoding",
        );

        let mut rebased = fixture_cache(false);
        rebased
            .class_bases
            .iter_mut()
            .find(|(class, _)| *class == mn("pkg.zeta.Zeta"))
            .expect("fixture should contain pkg.zeta.Zeta")
            .1
            .reverse();
        assert_ne!(
            encoded_bytes(&fixture_cache(false)),
            encoded_bytes(&rebased),
            "class base order is semantic (C3 linearization) and must survive encoding",
        );
    }
}

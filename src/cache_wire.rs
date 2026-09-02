/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::cache::CachedError;
use crate::cache::CachedExports;
use crate::cache::CachedModule;
use crate::cache::CachedModuleSafety;
use crate::cache::CachedReExport;
use crate::cache::CachedSafety;
use crate::cache::ConstructorCallees;
use crate::cache::LibraryCache;
use crate::effects::ImportedArgs;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashSetExt;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::MutatedParam;
use crate::module_safety::MutationCandidate;
use crate::module_safety::MutationCandidateSite;
use crate::module_safety::ParamPosition;

type NameId = u32;

/// Write buffer for the cache: caches are O(hundreds of MB), so a large buffer
/// minimizes write syscalls (matches the JSON writers in `commands`).
const WRITE_BUFFER_CAPACITY: usize = 1 << 20;

#[derive(Serialize, Deserialize)]
struct WireHeader {
    names: Vec<ModuleName>,
    exports: Vec<WireReExport>,
    class_bases: Vec<(NameId, Vec<NameId>)>,
    /// Class id, its metaclass id when a metaclass bit is set, and the callee
    /// mask. The callee FQNs themselves are reconstructible from these, so they
    /// stay out of the name table entirely.
    constructor_callees: Vec<(NameId, Option<NameId>, u8, Vec<NameId>)>,
}

#[derive(Serialize, Deserialize)]
struct WireModule {
    name: NameId,
    safety: WireSafety,
    imports: Vec<NameId>,
    missing_imports: Vec<NameId>,
    ambiguous_imports: Vec<NameId>,
    side_effect_imports: Vec<NameId>,
    function_safety: Vec<(String, WireFunctionSafetyInfo)>,
    mutation_candidates: Vec<WireMutationCandidate>,
}

#[derive(Serialize, Deserialize)]
enum WireSafety {
    Ok {
        errors: Vec<CachedError>,
        force_imports_eager_overrides: Vec<CachedError>,
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
struct WireMutationCandidate {
    callee: NameId,
    site: WireMutationCandidateSite,
    arg_offset: usize,
    imported_args: WireImportedArgs,
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
        for (class, bases) in &cache.class_bases {
            unique.insert(*class);
            unique.extend(bases.iter().copied());
        }
        for (class, recorded) in &cache.constructor_callees {
            unique.insert(*class);
            unique.extend(recorded.metaclass);
            unique.extend(recorded.extra.iter().copied());
        }

        ensure!(
            unique.len() <= NameId::MAX as usize,
            "cache contains too many distinct module names"
        );
        let names: Vec<ModuleName> = unique.into_iter().collect();
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
}

fn collect_module_names(module: &CachedModule, names: &mut AHashSet<ModuleName>) {
    names.insert(module.name);
    names.extend(module.imports.iter().copied());
    names.extend(module.missing_imports.iter().copied());
    names.extend(module.ambiguous_imports.iter().copied());
    names.extend(module.side_effect_imports.iter().copied());
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
}

pub(crate) fn write(cache: &LibraryCache, path: &Path) -> Result<()> {
    let table = NameTable::build(cache)?;
    let module_blobs: Vec<Vec<u8>> = cache
        .modules
        .par_iter()
        .map(|module| postcard::to_allocvec(&WireModule::encode(module, &table)))
        .collect::<std::result::Result<_, _>>()?;
    let exports = cache
        .exports
        .re_exports
        .iter()
        .map(|re_export| WireReExport::encode(re_export, &table.ids))
        .collect();
    let class_bases = cache
        .class_bases
        .iter()
        .map(|(class, bases)| {
            (
                table.id(*class),
                bases.iter().map(|base| table.id(*base)).collect(),
            )
        })
        .collect();
    let constructor_callees = cache
        .constructor_callees
        .iter()
        .map(|(class, recorded)| {
            (
                table.id(*class),
                recorded.metaclass.map(|metaclass| table.id(metaclass)),
                recorded.mask,
                recorded.extra.iter().map(|c| table.id(*c)).collect(),
            )
        })
        .collect();
    let header = WireHeader {
        names: table.names,
        exports,
        class_bases,
        constructor_callees,
    };
    let header_bytes = postcard::to_allocvec(&header)?;

    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_CAPACITY, file);
    write_len(&mut writer, header_bytes.len())?;
    writer.write_all(&header_bytes)?;
    write_len(&mut writer, module_blobs.len())?;
    for blob in module_blobs {
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
    Ok(LibraryCache {
        modules,
        exports,
        class_bases,
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
        Self {
            name: table.id(module.name),
            safety: WireSafety::encode(&module.safety, table),
            imports: module.imports.iter().map(|name| table.id(*name)).collect(),
            missing_imports: module
                .missing_imports
                .iter()
                .map(|name| table.id(*name))
                .collect(),
            ambiguous_imports: module
                .ambiguous_imports
                .iter()
                .map(|name| table.id(*name))
                .collect(),
            side_effect_imports: module
                .side_effect_imports
                .iter()
                .map(|name| table.id(*name))
                .collect(),
            function_safety: module
                .function_safety
                .iter()
                .map(|(name, info)| (name.clone(), WireFunctionSafetyInfo::encode(info, table)))
                .collect(),
            mutation_candidates: module
                .mutation_candidates
                .iter()
                .map(|candidate| WireMutationCandidate::encode(candidate, table))
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
        })
    }
}

impl WireSafety {
    fn encode(safety: &CachedSafety, table: &NameTable) -> Self {
        match safety {
            CachedSafety::Ok(safety) => Self::Ok {
                errors: safety.errors.clone(),
                force_imports_eager_overrides: safety.force_imports_eager_overrides.clone(),
                implicit_imports: safety
                    .implicit_imports
                    .iter()
                    .map(|name| table.id(*name))
                    .collect(),
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
        Self {
            verdict: info.verdict,
            missing_dep_callees: info
                .missing_dep_callees
                .iter()
                .map(|name| table.id(*name))
                .collect(),
            mutated_params: info
                .mutated_params
                .iter()
                .map(|param| WireMutatedParam {
                    name: table.id(param.name),
                    position: param.position,
                })
                .collect(),
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
            imported_args: WireImportedArgs {
                unsafe_arg_indices: candidate.imported_args.unsafe_arg_indices,
                unsafe_keyword_names: candidate
                    .imported_args
                    .unsafe_keyword_names
                    .iter()
                    .map(|name| table.id(*name))
                    .collect(),
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
    fn encode(re_export: &CachedReExport, ids: &AHashMap<ModuleName, NameId>) -> Self {
        let id = |name| {
            *ids.get(&name)
                .expect("all re-export module names should be in the wire name table")
        };
        Self {
            exported_module: id(re_export.exported_module),
            exported_attr: re_export.exported_attr.clone(),
            imported_module: id(re_export.imported_module),
            imported_attr: re_export.imported_attr.clone(),
        }
    }

    fn decode(self, names: &[ModuleName]) -> Result<CachedReExport> {
        Ok(CachedReExport {
            exported_module: decode_name(names, self.exported_module)?,
            exported_attr: self.exported_attr,
            imported_module: decode_name(names, self.imported_module)?,
            imported_attr: self.imported_attr,
        })
    }
}

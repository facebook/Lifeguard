/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Human-readable diagnostics: the pass/fail summary report and the per-module
//! verbose error writer.

use std::io::Write;

use itertools::Itertools;
use pyrefly_python::module_name::ModuleName;
use starlark_map::small_set::SmallSet;

use crate::errors::ErrorKind;
use crate::errors::ErrorMetadata;
use crate::errors::SafetyError;
use crate::hasher::AHashMap;
use crate::hasher::HashMapExt;
use crate::module_parser::ParsedModule;
use crate::module_safety::SafetyResult;
use crate::project::SafetyMap;
use crate::source_map::ModuleProvider;

pub struct AnalysisSummary {
    pub failing_modules: SmallSet<ModuleName>,
    pub passing_modules: SmallSet<ModuleName>,
    // Dictionary mapping (error kind, metadata) : num of occurrences
    pub aggregated_errors: AHashMap<(ErrorKind, ErrorMetadata), usize>,
}

impl AnalysisSummary {
    pub(crate) fn report(&self, load_imports_eagerly_count: usize) -> String {
        let mut error_vec: Vec<_> = self.aggregated_errors.iter().collect();

        let default_size = 20; // This could be made configurable
        let max_size = default_size.min(error_vec.len());

        error_vec.sort_by(|a, b| b.1.cmp(a.1));
        error_vec.truncate(max_size);

        let error_reports = error_vec
            .into_iter()
            .map(|((kind, metadata), prevalence)| {
                format!("{}, ({:?}, \"{}\")", prevalence, kind, metadata)
            })
            .join("\n");

        let total_modules = self.failing_modules.len() + self.passing_modules.len();
        let pass_rate_by_file = if total_modules > 0 {
            (self.passing_modules.len() as f64 / total_modules as f64) * 100.0
        } else {
            0.0
        };

        let avg_num_of_errors = if self.failing_modules.is_empty() {
            0.0
        } else {
            self.aggregated_errors.values().sum::<usize>() as f64
                / self.failing_modules.len() as f64
        };

        format!(
            "{}\nPASS RATE BY FILE %    | AVG NUM OF ERRORS IN FAILING MODULES\n{:.2} %                | {:.2}\nNum of failing files: {}\nNum of passing files: {}\nNum of load-imports-eagerly modules: {}",
            error_reports,
            pass_rate_by_file,
            avg_num_of_errors,
            self.failing_modules.len(),
            self.passing_modules.len(),
            load_imports_eagerly_count,
        )
    }

    pub(crate) fn print_diagnostics(&self) {
        for m in &self.passing_modules {
            println!("Passing: {:?}", m);
        }
        for m in &self.failing_modules {
            println!("Failing: {:?}", m);
        }
    }
}

/// Group `errors` by kind and write them under `header`: kinds sorted by name,
/// entries within each kind sorted by line number, each with a per-kind count.
/// Writes nothing when `errors` is empty.
fn write_grouped_errors<W: Write>(
    out: &mut W,
    header: &str,
    errors: &[SafetyError],
    module: &ParsedModule,
) -> std::io::Result<()> {
    if errors.is_empty() {
        return Ok(());
    }

    let mut by_kind: AHashMap<ErrorKind, Vec<(usize, &SafetyError)>> = AHashMap::new();
    for error in errors {
        let line = module.byte_to_line_number(error.range.start().into());
        by_kind.entry(error.kind).or_default().push((line, error));
    }

    let mut groups: Vec<(ErrorKind, Vec<(usize, &SafetyError)>)> = by_kind.into_iter().collect();
    groups.sort_by_cached_key(|(kind, _)| format!("{kind:?}"));

    writeln!(out, "{header}")?;
    for (kind, mut entries) in groups {
        entries.sort_by_key(|(line, _)| *line);
        writeln!(out, "{kind:?} ({})", entries.len())?;
        for (line, error) in entries {
            writeln!(out, "  Line {line} - {}", error.metadata.as_str())?;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Write all errors to a file. Parses each module on demand to get line numbers.
pub fn write_verbose<W: Write>(
    out: &mut W,
    safety_map: &SafetyMap,
    sources: &impl ModuleProvider,
) -> anyhow::Result<()> {
    writeln!(out, "# Lifeguard Verbose Output:")?;
    writeln!(
        out,
        "------------------------------------------------------------------------------"
    )?;

    let mut keys: Vec<ModuleName> = safety_map.iter().map(|entry| *entry.key()).collect();
    keys.sort();

    for module_name in &keys {
        let ast_result = sources.parse(module_name);

        let parsed_module = match &ast_result {
            Some(r) => match r.as_parsed() {
                Ok(m) => m,
                Err(_) => {
                    writeln!(out, "## {} ", module_name.as_str())?;
                    writeln!(out, "### Could not parse module\n")?;
                    continue;
                }
            },
            None => {
                writeln!(out, "## {} ", module_name.as_str())?;
                writeln!(out, "### Could not parse module\n")?;
                continue;
            }
        };

        writeln!(out, "## {} ", module_name.as_str())?;

        let Some(mut safety_ref) = safety_map.get_mut(module_name) else {
            continue;
        };
        let module_safety = match safety_ref.value_mut() {
            SafetyResult::Ok(safety) => safety,
            SafetyResult::AnalysisError(e) => {
                writeln!(out, "### Analysis Error")?;
                writeln!(out, "  {}", e)?;
                continue;
            }
        };

        if module_safety.errors.is_empty()
            && module_safety.force_imports_eager_overrides.is_empty()
            && module_safety.implicit_imports.is_empty()
        {
            writeln!(
                out,
                "### Lazy imports incompatibilities were not detected\n"
            )?;
            continue;
        }

        write_grouped_errors(out, "### Errors", &module_safety.errors, parsed_module)?;
        write_grouped_errors(
            out,
            "### Load Imports Eagerly",
            &module_safety.force_imports_eager_overrides,
            parsed_module,
        )?;

        if !module_safety.implicit_imports.is_empty() {
            writeln!(out, "### Implicit Imports")?;
            module_safety.implicit_imports.sort();
            for import in &module_safety.implicit_imports {
                writeln!(out, "  {}", import.as_str())?;
            }
        }

        writeln!(out)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use dashmap::DashMap;
    use ruff_text_size::TextRange;
    use ruff_text_size::TextSize;

    use super::*;
    use crate::module_safety::ModuleSafety;
    use crate::test_lib::TestSources;
    fn mn(s: &str) -> ModuleName {
        ModuleName::from_str(s)
    }

    fn make_error(kind: ErrorKind, metadata: &str, offset: u32) -> SafetyError {
        SafetyError::new(
            kind,
            metadata.to_string(),
            TextRange::new(TextSize::new(offset), TextSize::new(offset + 1)),
        )
    }

    // ---- write_verbose tests ----

    #[test]
    fn test_write_verbose_analysis_error() {
        let sources = TestSources::new(&[("broken", "x = 1\n")]);
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(
            mn("broken"),
            SafetyResult::AnalysisError(anyhow::anyhow!("something went wrong")),
        );

        let mut buf = Vec::new();
        write_verbose(&mut buf, &safety_map, &sources).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("## broken"));
        assert!(output.contains("### Analysis Error"));
        assert!(output.contains("something went wrong"));
    }

    #[test]
    fn test_write_verbose_no_errors() {
        let sources = TestSources::new(&[("clean", "x = 1\n")]);
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(mn("clean"), SafetyResult::Ok(ModuleSafety::new()));

        let mut buf = Vec::new();
        write_verbose(&mut buf, &safety_map, &sources).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("## clean"));
        assert!(output.contains("Lazy imports incompatibilities were not detected"));
    }

    #[test]
    fn test_write_verbose_with_errors() {
        let sources = TestSources::new(&[("bad", "some_func()\n")]);
        let safety_map: SafetyMap = DashMap::new();
        let mut safety = ModuleSafety::new();
        safety.add_error(make_error(ErrorKind::UnsafeFunctionCall, "some_func()", 0));
        safety_map.insert(mn("bad"), SafetyResult::Ok(safety));

        let mut buf = Vec::new();
        write_verbose(&mut buf, &safety_map, &sources).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("## bad"));
        assert!(output.contains("### Errors"));
        assert!(output.contains("UnsafeFunctionCall"));
        assert!(output.contains("some_func()"));
    }

    #[test]
    fn test_write_verbose_grouped_and_sorted() {
        // Test that errors are grouped by kind and sorted by line number within each group
        let source_code = "line1\nline2\nline3\nline4\nline5\nline6\n";
        let sources = TestSources::new(&[("module", source_code)]);
        let safety_map: SafetyMap = DashMap::new();
        let mut safety = ModuleSafety::new();

        // Add errors in non-sorted order, with multiple kinds
        // Line 6 - UnsafeFunctionCall
        safety.add_error(make_error(ErrorKind::UnsafeFunctionCall, "func_c()", 30));
        // Line 2 - ImportedModuleAssignment
        safety.add_error(make_error(ErrorKind::ImportedModuleAssignment, "sys", 6));
        // Line 7 - UnsafeFunctionCall
        safety.add_error(make_error(ErrorKind::UnsafeFunctionCall, "func_d()", 36));
        // Line 4 - ImportedModuleAssignment
        safety.add_error(make_error(ErrorKind::ImportedModuleAssignment, "os", 18));
        // Line 1 - UnsafeFunctionCall
        safety.add_error(make_error(ErrorKind::UnsafeFunctionCall, "func_a()", 0));

        safety_map.insert(mn("module"), SafetyResult::Ok(safety));

        let mut buf = Vec::new();
        write_verbose(&mut buf, &safety_map, &sources).unwrap();
        let output = String::from_utf8(buf).unwrap();

        // Verify grouping: kinds are sorted by name, so ImportedModuleAssignment
        // comes before UnsafeFunctionCall.
        let imported_pos = output.find("ImportedModuleAssignment").unwrap();
        let unsafe_pos = output.find("UnsafeFunctionCall").unwrap();
        assert!(
            imported_pos < unsafe_pos,
            "Errors should be grouped by kind"
        );

        // Verify counts are displayed
        assert!(output.contains("ImportedModuleAssignment (2)"));
        assert!(output.contains("UnsafeFunctionCall (3)"));

        // Verify sorting within ImportedModuleAssignment group
        let imported_section = &output[imported_pos..unsafe_pos];
        let sys_pos = imported_section.find("Line 2 - sys").unwrap();
        let os_pos = imported_section.find("Line 4 - os").unwrap();
        assert!(
            sys_pos < os_pos,
            "Errors should be sorted by line number within group"
        );

        // Verify sorting within UnsafeFunctionCall group
        let unsafe_section = &output[unsafe_pos..];
        let func_a_pos = unsafe_section.find("Line 1 - func_a()").unwrap();
        let func_c_pos = unsafe_section.find("Line 6 - func_c()").unwrap();
        let func_d_pos = unsafe_section.find("Line 7 - func_d()").unwrap();
        assert!(
            func_a_pos < func_c_pos && func_c_pos < func_d_pos,
            "Errors should be sorted by line number within group"
        );
    }
}

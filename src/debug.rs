/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use itertools::Itertools;
use tracing::debug;

use crate::exports::ExportType;
use crate::exports::Exports;
use crate::imports::ImportGraph;
use crate::module_effects::ModuleImportsMap;

/// Print exports table in sorted format for debugging.
/// Outputs one line per export in format:
///   class | global | re_export: <module_name>
pub fn print_exports(exports: &Exports) {
    let formatted_exports = exports.get_exports().map(|(name, export)| {
        let prefix = match export {
            ExportType::Class => "class",
            ExportType::Function => "function",
            ExportType::Global => "global",
        };
        format!("{}: {}", prefix, name.as_str())
    });
    let formatted_re_exports = exports
        .get_re_exports()
        .map(|(name, _)| format!("re-export: {}", name.as_module_name().as_str()));

    for line in formatted_exports.chain(formatted_re_exports).sorted() {
        println!("{}", line);
    }
}

pub fn print_import_cycles(imports: &ImportGraph) {
    let cycles = imports.graph.find_cycles();
    for c in cycles {
        println!("cycle {{");
        for m in imports.graph.cycle_names(&c) {
            println!("  {}", m);
        }
        println!("}}");
    }
}

/// Print the called_imports / pending_imports map from the ModuleEffects struct
pub fn print_module_imports_map(imports_map: &ModuleImportsMap) {
    for (scope, imports) in imports_map.iter().sorted_by_key(|(s, _)| s.as_str()) {
        println!("  {}:", scope.as_str());
        for import in imports.iter().sorted_by_key(|i| i.as_str()) {
            println!("    - {}", import.as_str());
        }
    }
}

/// Read a single `/proc/self/status` field by prefix (e.g. "VmHWM:"),
/// returning the trimmed line if present (Linux only).
fn read_proc_status_field(prefix: &str) -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|line| line.starts_with(prefix))
        .map(|line| line.trim().to_string())
}

/// Current resident set size (`VmRSS`) from /proc/self/status (Linux only).
fn read_proc_rss() -> Option<String> {
    read_proc_status_field("VmRSS:")
}

/// Peak resident set size (`VmHWM`) from /proc/self/status (Linux only).
fn read_proc_peak_memory() -> Option<String> {
    read_proc_status_field("VmHWM:")
}

/// Read and log VmRSS and VmHWM from /proc/self/status (Linux only).
pub fn report_memory(label: &str) {
    if let (Some(rss), Some(hwm)) = (read_proc_rss(), read_proc_peak_memory()) {
        debug!("[memory] {}: {} | {}", label, rss, hwm);
    }
}

/// Report peak resident set size from /proc/self/status.
pub fn report_peak_memory() {
    if let Some(hwm) = read_proc_peak_memory() {
        debug!("Peak memory (VmHWM): {}", hwm);
    }
}

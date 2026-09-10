use std::collections::{HashMap, HashSet};
use std::process::Command;

/// `cargo metadata` resolves the dependency graph for every platform by default, pulling in
/// packages that never build on this machine (Unix-only deps on Windows, etc). Narrow it down.
pub fn host_triple() -> Option<String> {
    let output = Command::new("rustc").arg("-vV").output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_string))
}

/// Exact unit count straight from cargo's own unit graph. `--unit-graph` is nightly-only, but
/// `RUSTC_BOOTSTRAP=1` is cargo's own sanctioned escape hatch for using it on stable.
pub fn exact_unit_count(cargo_args: &[&str], extra_args: &[String]) -> Option<usize> {
    let output = Command::new("cargo")
        .args(cargo_args)
        .args(["--unit-graph", "-Z", "unstable-options", "--message-format=json"])
        .args(extra_args)
        .env("RUSTC_BOOTSTRAP", "1")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let graph: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    graph.get("units")?.as_array().map(Vec::len)
}

/// Fallback for `exact_unit_count`: `metadata.packages` counts everything in the graph,
/// including optional deps whose gating feature isn't actually on, so walk the resolve graph
/// from the root and only follow edges the active feature set actually turns on.
pub fn build_closure_size(metadata: &cargo_metadata::Metadata) -> usize {
    let Some(resolve) = &metadata.resolve else {
        return metadata.packages.len();
    };
    let by_id: HashMap<&cargo_metadata::PackageId, &cargo_metadata::Node> =
        resolve.nodes.iter().map(|n| (&n.id, n)).collect();
    let pkg_by_id: HashMap<&cargo_metadata::PackageId, &cargo_metadata::Package> =
        metadata.packages.iter().map(|p| (&p.id, p)).collect();

    let mut stack: Vec<cargo_metadata::PackageId> = match &resolve.root {
        Some(root) => vec![root.clone()],
        None => metadata.workspace_members.clone(),
    };
    let mut seen: HashSet<cargo_metadata::PackageId> = HashSet::new();

    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let (Some(node), Some(pkg)) = (by_id.get(&id), pkg_by_id.get(&id)) else {
            continue;
        };

        for dep in &node.deps {
            let dev_only = !dep.dep_kinds.is_empty()
                && dep
                    .dep_kinds
                    .iter()
                    .all(|k| matches!(k.kind, cargo_metadata::DependencyKind::Development));
            if dev_only {
                continue;
            }

            let manifest_dep = pkg
                .dependencies
                .iter()
                .find(|d| d.rename.as_deref().unwrap_or(&d.name) == dep.name);
            let optional = manifest_dep.is_some_and(|d| d.optional);
            let active = !optional
                || node
                    .features
                    .iter()
                    .any(|f| f == &dep.name || f == &format!("dep:{}", dep.name));
            if active {
                stack.push(dep.pkg.clone());
            }
        }
    }
    seen.len()
}

pub fn dir_size(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

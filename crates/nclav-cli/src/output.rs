use nclav_reconciler::Change;
use nclav_domain::Enclave;
use nclav_store::EnclaveState;

/// Render a list of changes as human-readable diff output.
pub fn render_changes(changes: &[Change]) -> String {
    if changes.is_empty() {
        return "No changes.\n".to_string();
    }
    let mut out = String::new();
    for change in changes {
        let line = match change {
            Change::EnclaveCreated { id } => format!("+ enclave {}", id),
            Change::EnclaveUpdated { id } => format!("~ enclave {}", id),
            Change::EnclaveDeleted { id } => format!("- enclave {}", id),
            Change::PartitionCreated { enclave_id, partition_id } => {
                format!("  + partition {}/{}", enclave_id, partition_id)
            }
            Change::PartitionUpdated { enclave_id, partition_id } => {
                format!("  ~ partition {}/{}", enclave_id, partition_id)
            }
            Change::PartitionDeleted { enclave_id, partition_id } => {
                format!("  - partition {}/{}", enclave_id, partition_id)
            }
            Change::ExportWired { enclave_id, export_name } => {
                format!("  > export {}/{}", enclave_id, export_name)
            }
            Change::ImportWired { importer_enclave, alias } => {
                format!("  < import {}/{}", importer_enclave, alias)
            }
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Render live enclave states as a status table (used by `nclav status`).
pub fn render_status(states: &[EnclaveState]) -> String {
    if states.is_empty() {
        return "No enclaves in store. Run `nclav apply` first.\n".to_string();
    }
    let mut out = String::new();
    for s in states {
        out.push_str(&format!(
            "{:<30} [{}]\n",
            format!("{} ({})", s.desired.name, s.desired.id),
            s.meta.status,
        ));
        if let Some(err) = &s.meta.last_error {
            out.push_str(&format!("    ! {}\n", err.message));
        }
        for (pid, ps) in &s.partitions {
            out.push_str(&format!("  {:<28} [{}]\n", pid, ps.meta.status));
            if let Some(err) = &ps.meta.last_error {
                out.push_str(&format!("      ! {}\n", err.message));
            }
        }
    }
    out
}

/// Render the graph as Graphviz DOT from YAML-only data (no live state).
pub fn render_dot(enclaves: &[Enclave], filter_enclave: Option<&str>) -> String {
    let mut out = String::from("digraph nclav {\n  rankdir=LR;\n  node [shape=box];\n\n");

    for enc in enclaves {
        if let Some(f) = filter_enclave {
            if enc.id.as_str() != f {
                continue;
            }
        }
        out.push_str(&format!(
            "  subgraph cluster_{} {{\n    label=\"{}\";\n",
            sanitize(&enc.id.0),
            enc.name
        ));
        for part in &enc.partitions {
            out.push_str(&format!(
                "    \"{}:{}\" [label=\"{}\"];\n",
                enc.id.as_str(), part.id.as_str(), part.name
            ));
        }
        out.push_str("  }\n\n");
    }

    for enc in enclaves {
        for import in &enc.imports {
            out.push_str(&format!(
                "  \"{}\" -> \"{}\" [label=\"{}\"];\n",
                import.from.as_str(), enc.id.as_str(), import.export_name
            ));
        }
        for part in &enc.partitions {
            for import in &part.imports {
                out.push_str(&format!(
                    "  \"{}\" -> \"{}:{}\" [label=\"{}\"];\n",
                    import.from.as_str(), enc.id.as_str(), part.id.as_str(), import.export_name
                ));
            }
        }
    }

    out.push('}');
    out
}

/// Render the graph as Graphviz DOT from live store state, nodes coloured by status.
pub fn render_dot_live(states: &[EnclaveState], filter_enclave: Option<&str>) -> String {
    let mut out = String::from(
        "digraph nclav {\n  rankdir=LR;\n  node [shape=box, style=filled];\n\n",
    );

    for s in states {
        if let Some(f) = filter_enclave {
            if s.desired.id.as_str() != f {
                continue;
            }
        }
        let enc = &s.desired;
        out.push_str(&format!(
            "  subgraph cluster_{} {{\n    label=\"{} [{}]\";\n",
            sanitize(&enc.id.0), enc.name, s.meta.status,
        ));
        for part in &enc.partitions {
            let pstatus = s.partitions.get(&part.id)
                .map(|ps| ps.meta.status.to_string())
                .unwrap_or_else(|| "pending".to_string());
            out.push_str(&format!(
                "    \"{}:{}\" [label=\"{} [{}]\", fillcolor=\"{}\"];\n",
                enc.id.as_str(), part.id.as_str(), part.name, pstatus, status_color(&pstatus),
            ));
        }
        out.push_str("  }\n\n");
    }

    for s in states {
        let enc = &s.desired;
        for import in &enc.imports {
            out.push_str(&format!(
                "  \"{}\" -> \"{}\" [label=\"{}\"];\n",
                import.from.as_str(), enc.id.as_str(), import.export_name,
            ));
        }
        for part in &enc.partitions {
            for import in &part.imports {
                out.push_str(&format!(
                    "  \"{}\" -> \"{}:{}\" [label=\"{}\"];\n",
                    import.from.as_str(), enc.id.as_str(), part.id.as_str(), import.export_name,
                ));
            }
        }
    }

    out.push('}');
    out
}

/// Render graph as plain text from YAML-only data.
pub fn render_graph_text(enclaves: &[Enclave], filter_enclave: Option<&str>) -> String {
    let mut out = String::new();
    for enc in enclaves {
        if let Some(f) = filter_enclave {
            if enc.id.as_str() != f {
                continue;
            }
        }
        out.push_str(&format!("Enclave: {} ({})\n", enc.name, enc.id));
        for part in &enc.partitions {
            out.push_str(&format!("  Partition: {} ({})\n", part.name, part.id));
            if let Some(p) = &part.produces {
                out.push_str(&format!("    produces: {}\n", p));
            }
            for imp in &part.imports {
                out.push_str(&format!(
                    "    imports: {}.{} as {}\n",
                    imp.from, imp.export_name, imp.alias
                ));
            }
        }
        for exp in &enc.exports {
            out.push_str(&format!(
                "  Export: {} ({:?}) -> {:?}\n",
                exp.name, exp.export_type, exp.to
            ));
        }
        out.push('\n');
    }
    out
}

/// Render graph text from live store state, with status and resolved outputs.
pub fn render_graph_text_live(states: &[EnclaveState], filter_enclave: Option<&str>) -> String {
    let mut out = String::new();
    for s in states {
        if let Some(f) = filter_enclave {
            if s.desired.id.as_str() != f {
                continue;
            }
        }
        let enc = &s.desired;
        out.push_str(&format!(
            "Enclave: {} ({}) [{}]\n",
            enc.name, enc.id, s.meta.status
        ));
        if let Some(err) = &s.meta.last_error {
            out.push_str(&format!("  ! error: {}\n", err.message));
        }
        for part in &enc.partitions {
            let pstatus = s.partitions.get(&part.id)
                .map(|ps| ps.meta.status.to_string())
                .unwrap_or_else(|| "pending".to_string());
            out.push_str(&format!(
                "  Partition: {} ({}) [{}]\n",
                part.name, part.id, pstatus
            ));
            if let Some(ps) = s.partitions.get(&part.id) {
                if let Some(err) = &ps.meta.last_error {
                    out.push_str(&format!("    ! error: {}\n", err.message));
                }
                if let Some(p) = &part.produces {
                    out.push_str(&format!("    produces: {}\n", p));
                }
                for (k, v) in &ps.resolved_outputs {
                    out.push_str(&format!("    output {}: {}\n", k, v));
                }
            }
            for imp in &part.imports {
                out.push_str(&format!(
                    "    imports: {}.{} as {}\n",
                    imp.from, imp.export_name, imp.alias
                ));
            }
        }
        for exp in &enc.exports {
            out.push_str(&format!(
                "  Export: {} ({:?}) -> {:?}\n",
                exp.name, exp.export_type, exp.to
            ));
        }
        out.push('\n');
    }
    out
}

fn sanitize(s: &str) -> String {
    s.replace('-', "_").replace('.', "_")
}

fn status_color(status: &str) -> &'static str {
    match status {
        "active"                      => "#c8e6c9", // light green
        "error"                       => "#ffcdd2", // light red
        "degraded"                    => "#ffe0b2", // light orange
        "provisioning" | "updating"   => "#fff9c4", // light yellow
        "deleting" | "deleted"        => "#f5f5f5", // light grey
        _                             => "#ffffff", // white (pending)
    }
}

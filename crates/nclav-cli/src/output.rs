use nclav_reconciler::Change;
use nclav_domain::Enclave;

/// Render a list of changes as human-readable text.
pub fn render_changes(changes: &[Change]) -> String {
    if changes.is_empty() {
        return "No changes.".to_string();
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

/// Render the graph as Graphviz DOT.
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
                enc.id.as_str(),
                part.id.as_str(),
                part.name
            ));
        }

        out.push_str("  }\n\n");
    }

    // Cross-enclave edges
    for enc in enclaves {
        for import in &enc.imports {
            out.push_str(&format!(
                "  \"{}\" -> \"{}\" [label=\"{}\"];\n",
                import.from.as_str(),
                enc.id.as_str(),
                import.export_name
            ));
        }
        for part in &enc.partitions {
            for import in &part.imports {
                out.push_str(&format!(
                    "  \"{}\" -> \"{}:{}\" [label=\"{}\"];\n",
                    import.from.as_str(),
                    enc.id.as_str(),
                    part.id.as_str(),
                    import.export_name
                ));
            }
        }
    }

    out.push('}');
    out
}

/// Render the graph as plain text.
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

fn sanitize(s: &str) -> String {
    s.replace('-', "_").replace('.', "_")
}

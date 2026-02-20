use nclav_store::EnclaveState;

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

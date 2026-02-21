use std::collections::HashMap;

use nclav_domain::{Enclave, EnclaveId, ExportTarget, ExportType, PartitionId};
use petgraph::algo::is_cyclic_directed;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::{Deserialize, Serialize};

use crate::error::GraphError;

/// Opaque node identifier in the resolved graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

/// One cross-enclave import/export connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossEnclaveWiring {
    pub importer_enclave: EnclaveId,
    pub importer_partition: Option<PartitionId>,
    pub exporter_enclave: EnclaveId,
    pub export_name: String,
}

/// Result returned by [`validate`] on success.
#[derive(Debug)]
pub struct ResolvedGraph {
    /// Enclaves in topological order (no cross-enclave deps first).
    pub topo_order: Vec<NodeId>,
    /// All validated cross-enclave wiring.
    pub cross_enclave_wiring: Vec<CrossEnclaveWiring>,
}

/// Validate a fully-loaded set of enclaves.
///
/// Checks:
/// 1. Dangling imports (source enclave/export exists)
/// 2. Access control (`to:` permits the importer)
/// 3. Output contract (`declared_outputs ⊇ produces.required_outputs()`)
/// 4. Produces→export-type match
/// 5. Cycle detection
pub fn validate(enclaves: &[Enclave]) -> Result<ResolvedGraph, GraphError> {
    let by_id: HashMap<&EnclaveId, &Enclave> =
        enclaves.iter().map(|e| (&e.id, e)).collect();

    let mut errors: Vec<GraphError> = Vec::new();
    let mut wiring: Vec<CrossEnclaveWiring> = Vec::new();

    // --- Per-enclave checks ---
    for enc in enclaves {
        // Output contract per partition
        for part in &enc.partitions {
            if let Some(produces) = &part.produces {
                for key in produces.required_outputs() {
                    if !part.declared_outputs.iter().any(|o| o == key) {
                        errors.push(GraphError::MissingRequiredOutput {
                            partition: part.id.clone(),
                            produces_type: produces.to_string(),
                            key: key.to_string(),
                        });
                    }
                }
            }
        }

        // Produces→export type match for enclave-level exports
        for export in &enc.exports {
            let target_partition = enc
                .partitions
                .iter()
                .find(|p| p.id == export.target_partition);
            if let Some(part) = target_partition {
                if let Some(produces) = &part.produces {
                    let expected_export_type = ExportType::from(produces);
                    if expected_export_type != export.export_type {
                        errors.push(GraphError::ProducesExportMismatch {
                            partition: part.id.clone(),
                            produces_type: produces.to_string(),
                            export_name: export.name.clone(),
                            export_type: export.export_type.to_string(),
                        });
                    }
                }
            }
        }

        // Cross-enclave imports at enclave level
        for import in &enc.imports {
            match check_import(enc, import, &by_id) {
                Ok(w) => wiring.push(w),
                Err(e) => errors.push(e),
            }
        }

        // Cross-enclave imports at partition level
        for part in &enc.partitions {
            for import in &part.imports {
                match check_import_partition(enc, part.id.clone(), import, &by_id) {
                    Ok(w) => wiring.push(w),
                    Err(e) => errors.push(e),
                }
            }
        }
    }

    if !errors.is_empty() {
        if errors.len() == 1 {
            return Err(errors.remove(0));
        }
        return Err(GraphError::Multiple(errors));
    }

    // --- Cycle detection ---
    let mut graph: DiGraph<&EnclaveId, ()> = DiGraph::new();
    let node_map: HashMap<&EnclaveId, NodeIndex> = enclaves
        .iter()
        .map(|e| (&e.id, graph.add_node(&e.id)))
        .collect();

    // Add edges: exporter → importer ("exporter must be provisioned before importer").
    // Intra-enclave imports (same enclave) are valid wiring but produce no graph edge.
    for w in &wiring {
        if w.exporter_enclave == w.importer_enclave {
            continue;
        }
        let from = node_map[&w.exporter_enclave];
        let to = node_map[&w.importer_enclave];
        graph.add_edge(from, to, ());
    }

    if is_cyclic_directed(&graph) {
        return Err(GraphError::CycleDetected);
    }

    // Topological order
    let topo = petgraph::algo::toposort(&graph, None)
        .map_err(|_| GraphError::CycleDetected)?;
    let topo_order = topo
        .iter()
        .map(|idx| NodeId(graph[*idx].to_string()))
        .collect();

    Ok(ResolvedGraph {
        topo_order,
        cross_enclave_wiring: wiring,
    })
}

fn check_import(
    importer_enc: &Enclave,
    import: &nclav_domain::Import,
    by_id: &HashMap<&EnclaveId, &Enclave>,
) -> Result<CrossEnclaveWiring, GraphError> {
    check_import_partition(importer_enc, PartitionId::new(""), import, by_id).map(|mut w| {
        w.importer_partition = None;
        w
    })
}

fn check_import_partition(
    importer_enc: &Enclave,
    partition_id: PartitionId,
    import: &nclav_domain::Import,
    by_id: &HashMap<&EnclaveId, &Enclave>,
) -> Result<CrossEnclaveWiring, GraphError> {
    // 1. Source enclave exists
    let source = by_id
        .get(&import.from)
        .ok_or_else(|| GraphError::DanglingImportEnclave {
            importer: importer_enc.id.clone(),
            from: import.from.clone(),
        })?;

    // 2. Export exists on source
    let export = source
        .exports
        .iter()
        .find(|e| e.name == import.export_name)
        .ok_or_else(|| GraphError::DanglingImportExport {
            importer: importer_enc.id.clone(),
            from: import.from.clone(),
            export_name: import.export_name.clone(),
        })?;

    // 3. Access control
    let permitted = match &export.to {
        ExportTarget::Public | ExportTarget::AnyEnclave => true,
        ExportTarget::Vpn => true, // VPN access is topology-level, not name-checked here
        ExportTarget::Enclave(allowed_id) => allowed_id == &importer_enc.id,
        ExportTarget::Partition(_) => false, // partition-level, different kind of check
    };
    if !permitted {
        return Err(GraphError::AccessDenied {
            importer: importer_enc.id.clone(),
            from: import.from.clone(),
            export_name: import.export_name.clone(),
        });
    }

    let partition_id_opt = if partition_id.as_str().is_empty() {
        None
    } else {
        Some(partition_id)
    };

    Ok(CrossEnclaveWiring {
        importer_enclave: importer_enc.id.clone(),
        importer_partition: partition_id_opt,
        exporter_enclave: import.from.clone(),
        export_name: import.export_name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nclav_domain::*;

    fn make_enclave(id: &str, exports: Vec<Export>, partitions: Vec<Partition>) -> Enclave {
        Enclave {
            id: EnclaveId::new(id),
            name: id.to_string(),
            cloud: None,
            region: "local".to_string(),
            identity: None,
            network: None,
            dns: None,
            imports: vec![],
            exports,
            partitions,
        }
    }

    fn make_partition(id: &str, produces: Option<ProducesType>, declared_outputs: Vec<&str>) -> Partition {
        Partition {
            id: PartitionId::new(id),
            name: id.to_string(),
            produces,
            imports: vec![],
            exports: vec![],
            inputs: Default::default(),
            declared_outputs: declared_outputs.into_iter().map(String::from).collect(),
            backend: Default::default(),
        }
    }

    fn make_export(name: &str, target: &str, export_type: ExportType, to: ExportTarget) -> Export {
        Export {
            name: name.to_string(),
            target_partition: PartitionId::new(target),
            export_type,
            to,
            auth: AuthType::None,
            hostname: None,
            port: None,
        }
    }

    fn make_import(from: &str, export_name: &str, alias: &str) -> Import {
        Import {
            from: EnclaveId::new(from),
            export_name: export_name.to_string(),
            alias: alias.to_string(),
        }
    }

    #[test]
    fn valid_graph_passes() {
        let enc_a = make_enclave(
            "a",
            vec![make_export("a-http", "svc", ExportType::Http, ExportTarget::AnyEnclave)],
            vec![make_partition("svc", Some(ProducesType::Http), vec!["hostname", "port"])],
        );
        let mut enc_b = make_enclave("b", vec![], vec![]);
        enc_b.imports.push(make_import("a", "a-http", "upstream"));

        let result = validate(&[enc_a, enc_b]);
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn dangling_import_detected() {
        let mut enc = make_enclave("b", vec![], vec![]);
        enc.imports.push(make_import("nonexistent", "x", "x"));
        let result = validate(&[enc]);
        assert!(
            matches!(result, Err(GraphError::DanglingImportEnclave { .. })),
            "expected DanglingImportEnclave, got {:?}",
            result.err()
        );
    }

    #[test]
    fn dangling_export_detected() {
        let enc_a = make_enclave("a", vec![], vec![]);
        let mut enc_b = make_enclave("b", vec![], vec![]);
        enc_b.imports.push(make_import("a", "no-such-export", "x"));
        let result = validate(&[enc_a, enc_b]);
        assert!(
            matches!(result, Err(GraphError::DanglingImportExport { .. })),
            "expected DanglingImportExport, got {:?}",
            result.err()
        );
    }

    #[test]
    fn access_denied_detected() {
        let enc_a = make_enclave(
            "a",
            vec![make_export("svc", "svc", ExportType::Http, ExportTarget::Enclave(EnclaveId::new("allowed-only")))],
            vec![make_partition("svc", Some(ProducesType::Http), vec!["hostname", "port"])],
        );
        let mut enc_b = make_enclave("b", vec![], vec![]);
        enc_b.imports.push(make_import("a", "svc", "up"));
        let result = validate(&[enc_a, enc_b]);
        assert!(
            matches!(result, Err(GraphError::AccessDenied { .. })),
            "expected AccessDenied, got {:?}",
            result.err()
        );
    }

    #[test]
    fn missing_required_output_detected() {
        // http partition but missing "port" in declared_outputs
        let enc = make_enclave(
            "a",
            vec![],
            vec![make_partition("svc", Some(ProducesType::Http), vec!["hostname"])], // missing port
        );
        let result = validate(&[enc]);
        assert!(matches!(result, Err(GraphError::MissingRequiredOutput { .. })));
    }

    #[test]
    fn cycle_detected() {
        let mut enc_a = make_enclave(
            "a",
            vec![make_export("a-svc", "svc", ExportType::Http, ExportTarget::AnyEnclave)],
            vec![make_partition("svc", Some(ProducesType::Http), vec!["hostname", "port"])],
        );
        let mut enc_b = make_enclave(
            "b",
            vec![make_export("b-svc", "svc", ExportType::Http, ExportTarget::AnyEnclave)],
            vec![make_partition("svc", Some(ProducesType::Http), vec!["hostname", "port"])],
        );
        enc_a.imports.push(make_import("b", "b-svc", "b_up"));
        enc_b.imports.push(make_import("a", "a-svc", "a_up"));
        let result = validate(&[enc_a, enc_b]);
        assert!(matches!(result, Err(GraphError::CycleDetected)));
    }

    #[test]
    fn topo_sort_order() {
        // a has no deps; b imports from a — so a must come first
        let enc_a = make_enclave(
            "a",
            vec![make_export("a-svc", "svc", ExportType::Http, ExportTarget::AnyEnclave)],
            vec![make_partition("svc", Some(ProducesType::Http), vec!["hostname", "port"])],
        );
        let mut enc_b = make_enclave("b", vec![], vec![]);
        enc_b.imports.push(make_import("a", "a-svc", "up"));

        let graph = validate(&[enc_a, enc_b]).unwrap();
        let pos_a = graph.topo_order.iter().position(|n| n.0 == "a").unwrap();
        let pos_b = graph.topo_order.iter().position(|n| n.0 == "b").unwrap();
        assert!(pos_a < pos_b, "a must come before b in topo order");
    }
}

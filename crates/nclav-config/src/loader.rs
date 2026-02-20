use std::path::Path;

use nclav_domain::{
    AuthType, CloudTarget, DnsConfig, Enclave, EnclaveId, Export, ExportTarget, ExportType, Import,
    NetworkConfig, Partition, PartitionId, ProducesType,
};
use tracing::debug;

use crate::error::ConfigError;
use crate::raw::{RawEnclave, RawExport, RawExportTarget, RawImport, RawPartition};

/// Walk `dir` and load every enclave found.
///
/// Expected directory layout:
/// ```text
/// <dir>/
///   <enclave-name>/
///     config.yml          <- RawEnclave
///     <partition-name>/
///       config.yml        <- RawPartition
/// ```
pub fn load_enclaves(dir: &Path) -> Result<Vec<Enclave>, ConfigError> {
    let mut enclaves = Vec::new();

    let entries = std::fs::read_dir(dir).map_err(|e| ConfigError::Io {
        path: dir.display().to_string(),
        source: e,
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| ConfigError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // Each subdirectory may contain more subdirs (e.g. product-a/dev/)
        // We recursively look for config.yml files that describe enclaves.
        collect_enclaves(&path, &mut enclaves)?;
    }

    Ok(enclaves)
}

fn collect_enclaves(dir: &Path, out: &mut Vec<Enclave>) -> Result<(), ConfigError> {
    let config_path = dir.join("config.yml");
    if config_path.exists() {
        // Try to parse as an enclave config
        let content = std::fs::read_to_string(&config_path).map_err(|e| ConfigError::Io {
            path: config_path.display().to_string(),
            source: e,
        })?;
        // Check if it looks like an enclave (has `cloud` field)
        if let Ok(raw) = serde_yaml::from_str::<RawEnclave>(&content) {
            debug!("Loading enclave from {}", config_path.display());
            let enclave = convert_enclave(raw, dir, &config_path)?;
            out.push(enclave);
            return Ok(());
        }
    }

    // Recurse into subdirectories
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_enclaves(&path, out)?;
            }
        }
    }

    Ok(())
}

fn convert_enclave(
    raw: RawEnclave,
    dir: &Path,
    config_path: &Path,
) -> Result<Enclave, ConfigError> {
    let cloud = parse_cloud(&raw.cloud, config_path)?;
    let imports = raw
        .imports
        .into_iter()
        .map(|i| convert_import(i, config_path))
        .collect::<Result<Vec<_>, _>>()?;
    let exports = raw
        .exports
        .into_iter()
        .map(|e| convert_export(e, config_path))
        .collect::<Result<Vec<_>, _>>()?;

    // Load partitions: each name in raw.partitions is a subdirectory of dir
    let mut partitions = Vec::new();
    for part_name in &raw.partitions {
        let part_dir = dir.join(part_name);
        let part_config = part_dir.join("config.yml");
        if !part_config.exists() {
            return Err(ConfigError::Conversion {
                path: part_config.display().to_string(),
                message: format!("partition config not found for '{}'", part_name),
            });
        }
        let content =
            std::fs::read_to_string(&part_config).map_err(|e| ConfigError::Io {
                path: part_config.display().to_string(),
                source: e,
            })?;
        let raw_part: RawPartition =
            serde_yaml::from_str(&content).map_err(|e| ConfigError::YamlParse {
                path: part_config.display().to_string(),
                source: e,
            })?;
        partitions.push(convert_partition(raw_part, &part_config)?);
    }

    // If no explicit partition list, scan subdirectories
    if raw.partitions.is_empty() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let part_config = path.join("config.yml");
                if !part_config.exists() {
                    continue;
                }
                let content =
                    std::fs::read_to_string(&part_config).map_err(|e| ConfigError::Io {
                        path: part_config.display().to_string(),
                        source: e,
                    })?;
                // Try to parse as partition
                if let Ok(raw_part) = serde_yaml::from_str::<RawPartition>(&content) {
                    if raw_part.produces.is_some()
                        || !raw_part.imports.is_empty()
                        || !raw_part.exports.is_empty()
                        || !raw_part.declared_outputs.is_empty()
                    {
                        partitions.push(convert_partition(raw_part, &part_config)?);
                    }
                }
            }
        }
    }

    let network = raw.network.map(|n| NetworkConfig {
        vpc_cidr: n.vpc_cidr,
        subnets: n.subnets,
    });

    let dns = raw.dns.map(|d| DnsConfig { zone: d.zone });

    Ok(Enclave {
        id: EnclaveId::new(&raw.id),
        name: raw.name,
        cloud,
        region: raw.region,
        identity: raw.identity,
        network,
        dns,
        imports,
        exports,
        partitions,
    })
}

fn convert_partition(raw: RawPartition, path: &Path) -> Result<Partition, ConfigError> {
    let produces = raw.produces.as_deref().map(|s| parse_produces(s, path)).transpose()?;
    let imports = raw
        .imports
        .into_iter()
        .map(|i| convert_import(i, path))
        .collect::<Result<Vec<_>, _>>()?;
    let exports = raw
        .exports
        .into_iter()
        .map(|e| convert_export(e, path))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Partition {
        id: PartitionId::new(&raw.id),
        name: raw.name,
        produces,
        imports,
        exports,
        inputs: raw.inputs,
        declared_outputs: raw.declared_outputs,
    })
}

fn convert_import(raw: RawImport, _path: &Path) -> Result<Import, ConfigError> {
    Ok(Import {
        from: EnclaveId::new(&raw.from),
        export_name: raw.export_name,
        alias: raw.alias,
    })
}

fn convert_export(raw: RawExport, path: &Path) -> Result<Export, ConfigError> {
    let export_type = parse_export_type(&raw.export_type, path)?;
    let auth = parse_auth(&raw.auth, path)?;
    let to = convert_export_target(raw.to, path)?;

    Ok(Export {
        name: raw.name,
        target_partition: PartitionId::new(&raw.target_partition),
        export_type,
        to,
        auth,
        hostname: raw.hostname,
        port: raw.port,
    })
}

fn convert_export_target(raw: RawExportTarget, path: &Path) -> Result<ExportTarget, ConfigError> {
    match raw {
        RawExportTarget::Simple(s) => match s.as_str() {
            "public" => Ok(ExportTarget::Public),
            "any_enclave" | "any-enclave" => Ok(ExportTarget::AnyEnclave),
            "vpn" => Ok(ExportTarget::Vpn),
            other => Err(ConfigError::Conversion {
                path: path.display().to_string(),
                message: format!("unknown export target '{}'", other),
            }),
        },
        RawExportTarget::Enclave { enclave } => {
            Ok(ExportTarget::Enclave(EnclaveId::new(enclave)))
        }
        RawExportTarget::Partition { partition } => {
            Ok(ExportTarget::Partition(PartitionId::new(partition)))
        }
    }
}

fn parse_cloud(s: &str, path: &Path) -> Result<CloudTarget, ConfigError> {
    match s {
        "local" => Ok(CloudTarget::Local),
        "gcp"   => Ok(CloudTarget::Gcp),
        "azure" => Ok(CloudTarget::Azure),
        other => Err(ConfigError::Conversion {
            path: path.display().to_string(),
            message: format!("unknown cloud target '{}'", other),
        }),
    }
}

fn parse_produces(s: &str, path: &Path) -> Result<ProducesType, ConfigError> {
    match s {
        "http" => Ok(ProducesType::Http),
        "tcp" => Ok(ProducesType::Tcp),
        "queue" => Ok(ProducesType::Queue),
        other => Err(ConfigError::Conversion {
            path: path.display().to_string(),
            message: format!("unknown produces type '{}'", other),
        }),
    }
}

fn parse_export_type(s: &str, path: &Path) -> Result<ExportType, ConfigError> {
    match s {
        "http" => Ok(ExportType::Http),
        "tcp" => Ok(ExportType::Tcp),
        "queue" => Ok(ExportType::Queue),
        other => Err(ConfigError::Conversion {
            path: path.display().to_string(),
            message: format!("unknown export type '{}'", other),
        }),
    }
}

fn parse_auth(s: &str, path: &Path) -> Result<AuthType, ConfigError> {
    match s {
        "none" => Ok(AuthType::None),
        "token" => Ok(AuthType::Token),
        "oauth" => Ok(AuthType::Oauth),
        "mtls" => Ok(AuthType::Mtls),
        "native" => Ok(AuthType::Native),
        other => Err(ConfigError::Conversion {
            path: path.display().to_string(),
            message: format!("unknown auth type '{}'", other),
        }),
    }
}

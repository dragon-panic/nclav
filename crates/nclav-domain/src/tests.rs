#[cfg(test)]
mod tests {
    use crate::types::*;

    #[test]
    fn http_auth_matrix() {
        assert!(ExportType::Http.is_auth_compatible(&AuthType::None));
        assert!(ExportType::Http.is_auth_compatible(&AuthType::Token));
        assert!(ExportType::Http.is_auth_compatible(&AuthType::Oauth));
        assert!(ExportType::Http.is_auth_compatible(&AuthType::Mtls));
        assert!(!ExportType::Http.is_auth_compatible(&AuthType::Native));
    }

    #[test]
    fn tcp_auth_matrix() {
        assert!(ExportType::Tcp.is_auth_compatible(&AuthType::None));
        assert!(ExportType::Tcp.is_auth_compatible(&AuthType::Mtls));
        assert!(ExportType::Tcp.is_auth_compatible(&AuthType::Native));
        assert!(!ExportType::Tcp.is_auth_compatible(&AuthType::Token));
        assert!(!ExportType::Tcp.is_auth_compatible(&AuthType::Oauth));
    }

    #[test]
    fn queue_auth_matrix() {
        assert!(ExportType::Queue.is_auth_compatible(&AuthType::None));
        assert!(ExportType::Queue.is_auth_compatible(&AuthType::Token));
        assert!(ExportType::Queue.is_auth_compatible(&AuthType::Native));
        assert!(!ExportType::Queue.is_auth_compatible(&AuthType::Oauth));
        assert!(!ExportType::Queue.is_auth_compatible(&AuthType::Mtls));
    }

    #[test]
    fn http_required_outputs() {
        let outputs = ProducesType::Http.required_outputs();
        assert!(outputs.contains(&"hostname"));
        assert!(outputs.contains(&"port"));
    }

    #[test]
    fn tcp_required_outputs() {
        let outputs = ProducesType::Tcp.required_outputs();
        assert!(outputs.contains(&"hostname"));
        assert!(outputs.contains(&"port"));
    }

    #[test]
    fn queue_required_outputs() {
        let outputs = ProducesType::Queue.required_outputs();
        assert!(outputs.contains(&"queue_url"));
    }

    #[test]
    fn produces_to_export_type_conversion() {
        assert_eq!(ExportType::from(&ProducesType::Http), ExportType::Http);
        assert_eq!(ExportType::from(&ProducesType::Tcp), ExportType::Tcp);
        assert_eq!(ExportType::from(&ProducesType::Queue), ExportType::Queue);
    }
}

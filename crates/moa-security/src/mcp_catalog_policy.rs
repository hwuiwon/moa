//! Deterministic connector-policy checks for a staged MCP catalog candidate.
//!
//! Catalog discovery produces a *candidate*, not a replacement. Before a
//! candidate connector may be activated it has to clear the checks in this
//! module, which are the security-policy half of staging: they look only at the
//! operator's declared configuration and at facts the handshake already
//! established, never at model behaviour and never at a second network call.
//!
//! The split between [`ConnectorPolicyDefect`] and [`ConnectorPolicyWarning`] is
//! the whole point of the module. A defect is a deterministic policy violation
//! and quarantines the connector, so the last-known-good catalog keeps serving.
//! A warning is a real observation that is *not* a deterministic contract
//! failure — an operator intent the negotiated protocol cannot honour, or a
//! connector that published nothing — and it must never withdraw a connector on
//! its own. Nothing here consults a model, because a stochastic signal cannot be
//! allowed to take a working integration offline.

use moa_config::McpServerConfig;
use moa_core::types::security::SensitivityClass;

/// Facts about one candidate connector available without a second network call.
///
/// Deliberately not the discovered tool list: policy is decided from the
/// operator's declared configuration plus the handshake result, so this check
/// cannot become schema validation by accident.
#[derive(Debug, Clone, Copy)]
pub struct ConnectorCandidateFacts<'a> {
    /// The operator's declared configuration for this connector.
    pub server: &'a McpServerConfig,
    /// Protocol revision the connector selected during `initialize`.
    pub negotiated_protocol_version: &'a str,
    /// Number of tools the candidate discovery pass listed.
    pub discovered_tools: usize,
}

/// A deterministic connector-policy violation that quarantines a candidate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorPolicyDefect {
    /// The connector is allowlisted for sensitive data over an unencrypted URL.
    ///
    /// The egress guard would happily permit the class, because the allowlist
    /// says so; the transport is what makes the permission unsound. Catching it
    /// at staging is the only place it is still cheap.
    #[error(
        "connector '{server}' is allowlisted for {class} data over insecure transport '{scheme}'"
    )]
    SensitiveEgressOverInsecureTransport {
        /// Configured connector name.
        server: String,
        /// The sensitive class the operator allowlisted.
        class: SensitivityClass,
        /// URL scheme the connector is configured with.
        scheme: String,
    },
    /// The connector URL is not a scheme this deployment can dispatch over.
    #[error("connector '{server}' declares unsupported URL scheme '{scheme}'")]
    UnsupportedTransport {
        /// Configured connector name.
        server: String,
        /// URL scheme the connector is configured with.
        scheme: String,
    },
}

/// A non-blocking connector observation recorded alongside an activation.
///
/// Warnings are the recorded answer to "structurally valid, behaviourally
/// suspicious". They are visible in the activation report and in logs, and they
/// deliberately have no path to withdrawing a connector.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorPolicyWarning {
    /// The connector selected a protocol revision MOA cannot order.
    ///
    /// Not a defect. An unorderable revision means MOA cannot conclude that the
    /// connector supports anything optional, and every consequence of that
    /// already fails closed — tool annotations go untrusted and every tool from
    /// the connector is treated as non-idempotent. The connector's tools still
    /// work, so withdrawing them would be a self-inflicted outage over a
    /// malformed version string.
    #[error(
        "connector '{server}' negotiated an unrecognized MCP protocol revision '{protocol_version}'"
    )]
    UnrecognizedProtocolRevision {
        /// Configured connector name.
        server: String,
        /// The revision string the connector returned.
        protocol_version: String,
    },
    /// The operator asked to trust tool annotations the revision cannot carry.
    ///
    /// Not a defect: MOA already degrades every tool from this connector to
    /// non-idempotent, so the connector is safe to serve — the operator's
    /// intent simply has no effect.
    #[error(
        "connector '{server}' trusts tool annotations but revision '{protocol_version}' does not carry them"
    )]
    AnnotationTrustUnsupportedByRevision {
        /// Configured connector name.
        server: String,
        /// Revision the connector negotiated.
        protocol_version: String,
    },
    /// The connector handshake succeeded but it published no tools.
    #[error("connector '{server}' published an empty tool catalog")]
    EmptyToolCatalog {
        /// Configured connector name.
        server: String,
    },
}

/// Outcome of the deterministic policy checks for one candidate connector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectorPolicyReport {
    /// Violations that quarantine the candidate connector.
    pub defects: Vec<ConnectorPolicyDefect>,
    /// Observations recorded with the activation without blocking it.
    pub warnings: Vec<ConnectorPolicyWarning>,
}

impl ConnectorPolicyReport {
    /// Returns whether this candidate connector may be activated.
    #[must_use]
    pub fn is_activatable(&self) -> bool {
        self.defects.is_empty()
    }
}

/// First MCP revision that carries tool annotations.
const ANNOTATION_REVISION: &str = "2025-03-26";

/// Runs every deterministic policy check for one candidate connector.
///
/// Total: it returns every violation and observation rather than the first, so a
/// quarantine report names everything an operator has to fix instead of
/// revealing one problem per staging pass.
#[must_use]
pub fn check_connector_policy(facts: ConnectorCandidateFacts<'_>) -> ConnectorPolicyReport {
    let server = facts.server.name.as_str();
    let mut report = ConnectorPolicyReport::default();

    let revision = parse_protocol_revision(facts.negotiated_protocol_version);
    if revision.is_none() {
        report
            .warnings
            .push(ConnectorPolicyWarning::UnrecognizedProtocolRevision {
                server: server.to_string(),
                protocol_version: facts.negotiated_protocol_version.to_string(),
            });
    }

    let scheme = url_scheme(&facts.server.url);
    match scheme.as_str() {
        "https" => {}
        "http" => {
            // `SensitivityClass::None` is not listed: it is what every allowlist
            // permits by default, so treating it as sensitive would quarantine
            // every plain-HTTP connector including local development ones.
            for class in [
                SensitivityClass::Pii,
                SensitivityClass::Phi,
                SensitivityClass::Restricted,
            ] {
                if facts.server.allowed_data_classes.contains(&class) {
                    report.defects.push(
                        ConnectorPolicyDefect::SensitiveEgressOverInsecureTransport {
                            server: server.to_string(),
                            class,
                            scheme: scheme.clone(),
                        },
                    );
                }
            }
        }
        _ => report
            .defects
            .push(ConnectorPolicyDefect::UnsupportedTransport {
                server: server.to_string(),
                scheme: scheme.clone(),
            }),
    }

    if facts.server.trust_tool_annotations
        && !revision.is_some_and(|revision| revision >= annotation_revision())
    {
        report.warnings.push(
            ConnectorPolicyWarning::AnnotationTrustUnsupportedByRevision {
                server: server.to_string(),
                protocol_version: facts.negotiated_protocol_version.to_string(),
            },
        );
    }

    if facts.discovered_tools == 0 {
        report
            .warnings
            .push(ConnectorPolicyWarning::EmptyToolCatalog {
                server: server.to_string(),
            });
    }

    report
}

/// Parses an MCP revision date, which is the revision's total order.
fn parse_protocol_revision(protocol_version: &str) -> Option<chrono::NaiveDate> {
    let bytes = protocol_version.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    chrono::NaiveDate::parse_from_str(protocol_version, "%Y-%m-%d").ok()
}

/// Returns the annotation-carrying revision boundary.
fn annotation_revision() -> chrono::NaiveDate {
    parse_protocol_revision(ANNOTATION_REVISION)
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(2025, 3, 26).unwrap_or_default())
}

/// Returns the lowercase URL scheme, or an empty string when there is none.
fn url_scheme(url: &str) -> String {
    url.split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use moa_config::{McpDiscoveryMode, McpServerConfig};
    use moa_core::types::security::SensitivityClass;

    use super::{
        ConnectorCandidateFacts, ConnectorPolicyDefect, ConnectorPolicyWarning,
        check_connector_policy,
    };

    fn server(url: &str) -> McpServerConfig {
        McpServerConfig {
            name: "catalog".to_string(),
            url: url.to_string(),
            required: false,
            discovery: McpDiscoveryMode::Eager,
            allowed_data_classes: Vec::new(),
            credentials: None,
            trust_tool_annotations: false,
        }
    }

    #[test]
    fn a_healthy_https_connector_has_no_defects_or_warnings_offline() {
        // Pins: the checks are opt-in violations, not a gate every ordinary
        // connector has to argue its way past. A conforming connector must
        // activate with an empty report, otherwise staging would quarantine
        // every deployment on its first refresh.
        let server = server("https://connector.example/mcp");
        let report = check_connector_policy(ConnectorCandidateFacts {
            server: &server,
            negotiated_protocol_version: "2025-03-26",
            discovered_tools: 3,
        });

        assert!(report.is_activatable());
        assert_eq!(report.defects, Vec::new());
        assert_eq!(report.warnings, Vec::new());
    }

    #[test]
    fn an_unrecognized_protocol_revision_warns_without_quarantining_offline() {
        // Pins: an unorderable revision is a warning, not a defect. Every
        // consequence of not knowing the revision already fails closed — the
        // connector's tools become non-idempotent and its annotations untrusted —
        // so quarantining it would take a working integration offline over a
        // malformed version string. Mutating this to a defect makes
        // `discovery_trusts_idempotent_hint_only_for_explicit_capable_server`
        // fail, which is the behaviour this preserves.
        let server = server("https://connector.example/mcp");
        let report = check_connector_policy(ConnectorCandidateFacts {
            server: &server,
            negotiated_protocol_version: "2025-04-31",
            discovered_tools: 1,
        });

        assert!(report.is_activatable(), "{:?}", report.defects);
        assert!(
            report
                .warnings
                .contains(&ConnectorPolicyWarning::UnrecognizedProtocolRevision {
                    server: "catalog".to_string(),
                    protocol_version: "2025-04-31".to_string(),
                }),
            "unexpected warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn sensitive_data_classes_over_plain_http_quarantine_every_named_class_offline() {
        // Pins: the egress allowlist and the transport are separate facts. The
        // guard would permit the class because the operator listed it, so the
        // only place the unencrypted transport can be caught is staging — and
        // every allowlisted sensitive class is named, not just the first.
        let mut config = server("http://connector.internal/mcp");
        config.allowed_data_classes = vec![SensitivityClass::Phi, SensitivityClass::Restricted];
        let report = check_connector_policy(ConnectorCandidateFacts {
            server: &config,
            negotiated_protocol_version: "2025-03-26",
            discovered_tools: 1,
        });

        assert!(!report.is_activatable());
        assert_eq!(
            report.defects,
            vec![
                ConnectorPolicyDefect::SensitiveEgressOverInsecureTransport {
                    server: "catalog".to_string(),
                    class: SensitivityClass::Phi,
                    scheme: "http".to_string(),
                },
                ConnectorPolicyDefect::SensitiveEgressOverInsecureTransport {
                    server: "catalog".to_string(),
                    class: SensitivityClass::Restricted,
                    scheme: "http".to_string(),
                },
            ]
        );
    }

    #[test]
    fn plain_http_without_sensitive_classes_stays_activatable_offline() {
        // Pins: the transport defect is about the declared data classes, not
        // about `http` itself. Local development and fixture connectors are
        // plain HTTP and must keep activating.
        let config = server("http://127.0.0.1:8080");
        let report = check_connector_policy(ConnectorCandidateFacts {
            server: &config,
            negotiated_protocol_version: "2025-03-26",
            discovered_tools: 2,
        });

        assert!(report.is_activatable(), "{:?}", report.defects);
    }

    #[test]
    fn unsupported_transport_schemes_are_defects_offline() {
        // Pins: a scheme the deployment cannot dispatch over is caught at
        // staging rather than surfacing as a per-call transport error later.
        let config = server("ftp://connector.example/mcp");
        let report = check_connector_policy(ConnectorCandidateFacts {
            server: &config,
            negotiated_protocol_version: "2025-03-26",
            discovered_tools: 1,
        });

        assert_eq!(
            report.defects,
            vec![ConnectorPolicyDefect::UnsupportedTransport {
                server: "catalog".to_string(),
                scheme: "ftp".to_string(),
            }]
        );
    }

    #[test]
    fn unhonourable_annotation_trust_warns_without_quarantining_offline() {
        // Pins: an operator intent the negotiated revision cannot carry is a
        // warning. MOA already degrades these tools to non-idempotent, so the
        // connector is safe to serve, and quarantining it would take a working
        // integration offline over a configuration nuance.
        let mut config = server("https://connector.example/mcp");
        config.trust_tool_annotations = true;
        let report = check_connector_policy(ConnectorCandidateFacts {
            server: &config,
            negotiated_protocol_version: "2024-11-05",
            discovered_tools: 1,
        });

        assert!(report.is_activatable());
        assert_eq!(
            report.warnings,
            vec![
                ConnectorPolicyWarning::AnnotationTrustUnsupportedByRevision {
                    server: "catalog".to_string(),
                    protocol_version: "2024-11-05".to_string(),
                }
            ]
        );
    }

    #[test]
    fn an_empty_tool_catalog_warns_without_quarantining_offline() {
        // Pins: a connector that published nothing keeps whatever it was
        // serving. Treating "zero tools this pass" as a defect would let a
        // connector mid-deploy withdraw its own last-known-good tools.
        let config = server("https://connector.example/mcp");
        let report = check_connector_policy(ConnectorCandidateFacts {
            server: &config,
            negotiated_protocol_version: "2025-06-18",
            discovered_tools: 0,
        });

        assert!(report.is_activatable());
        assert_eq!(
            report.warnings,
            vec![ConnectorPolicyWarning::EmptyToolCatalog {
                server: "catalog".to_string(),
            }]
        );
    }
}

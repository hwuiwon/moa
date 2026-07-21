//! `[kms]` configuration for envelope-encryption key management.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default root-key generation required by a deployment.
fn default_required_generation() -> String {
    "primary".to_string()
}

/// Default Kubernetes Secret mount containing root-key generation files.
fn default_root_key_dir() -> PathBuf {
    PathBuf::from("/var/run/secrets/moa-kms/root-keys")
}

/// Key-management provider configuration.
///
/// Selects which [`KeyManagementProvider`](crate) backs envelope encryption and
/// crypto-shred. Root-key material never lives in configuration: Kubernetes
/// mounts one base64 file per generation into [`Self::root_key_dir`].
///
/// Production deployments that enable restricted-row encryption MUST select
/// [`KmsProviderKind::Postgres`] (or another persistent provider). The default,
/// [`KmsProviderKind::Local`], keeps in-process keys that do not survive a
/// restart — correct for development and tests, unsafe for real data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KmsConfig {
    /// Selected key-management provider.
    pub provider: KmsProviderKind,
    /// Directory containing base64 root-key files named by generation.
    #[serde(default = "default_root_key_dir")]
    pub root_key_dir: PathBuf,
    /// Generation this pod requires the database to have active before it is
    /// compatible and ready.
    #[serde(default = "default_required_generation")]
    pub required_generation: String,
    /// Development/test opt-in permitting a non-durable (ephemeral) provider such
    /// as [`KmsProviderKind::Local`] to back envelope encryption.
    ///
    /// Defaults to `false`, so the composition root FAILS CLOSED: a deployment
    /// that installs an ephemeral provider — whose keys are lost on restart —
    /// cannot boot, because sealing persisted restricted/PHI data with such keys
    /// would silently render that data unrecoverable. Set to `true`
    /// (`MOA_KMS_ALLOW_EPHEMERAL=true`) only for local development and tests,
    /// where losing the keys on restart is acceptable. A durable provider (for
    /// example [`KmsProviderKind::Postgres`]) is always permitted regardless of
    /// this flag.
    #[serde(default)]
    pub allow_ephemeral: bool,
}

impl Default for KmsConfig {
    fn default() -> Self {
        Self {
            provider: KmsProviderKind::default(),
            root_key_dir: default_root_key_dir(),
            required_generation: default_required_generation(),
            allow_ephemeral: false,
        }
    }
}

/// Supported key-management provider kinds.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum KmsProviderKind {
    /// In-process keys held in memory; lost on restart. Development and tests
    /// only.
    #[default]
    Local,
    /// Persistent, self-hosted provider that stores wrapped KEKs in Postgres.
    Postgres,
}

impl KmsProviderKind {
    /// Return the serialized configuration value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_local_with_typed_keyring_settings_offline() {
        // Pins: an unset section has a typed Secret mount and required generation.
        let config = KmsConfig::default();
        assert_eq!(config.provider, KmsProviderKind::Local);
        assert_eq!(
            config.root_key_dir,
            PathBuf::from("/var/run/secrets/moa-kms/root-keys")
        );
        assert_eq!(config.required_generation, "primary");
        // Ephemeral sealing is opt-in: the default fails closed at the
        // composition root rather than silently encrypting with lost-on-restart
        // keys.
        assert!(!config.allow_ephemeral);
    }

    #[test]
    fn allow_ephemeral_opt_in_deserializes_offline() {
        // Pins: allow_ephemeral is an explicit dev/test opt-in that config (and the
        // MOA_KMS_ALLOW_EPHEMERAL overlay) can flip on without affecting the
        // fail-closed default.
        let config: KmsConfig =
            serde_json::from_value(serde_json::json!({ "allow_ephemeral": true })).expect("parse");
        assert!(config.allow_ephemeral);
        assert_eq!(config.provider, KmsProviderKind::Local);
    }

    #[test]
    fn omitted_required_generation_deserializes_to_primary_offline() {
        // Pins: a Postgres section without an explicit generation requires primary.
        let config: KmsConfig =
            serde_json::from_value(serde_json::json!({ "provider": "postgres" })).expect("parse");
        assert_eq!(config.provider, KmsProviderKind::Postgres);
        assert_eq!(config.required_generation, "primary");
    }
}

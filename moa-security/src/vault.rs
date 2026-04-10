//! Encrypted credential vault implementations for local MOA deployments.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs::Permissions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use age::secrecy::SecretString;
use async_trait::async_trait;
use moa_core::{Credential, CredentialVault, MoaError, Result};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tokio::fs;
use tracing::debug;
use uuid::Uuid;

type VaultStore = BTreeMap<String, BTreeMap<String, Credential>>;

/// Local encrypted file vault backed by an age-encrypted JSON document.
#[derive(Debug, Clone)]
pub struct FileVault {
    path: PathBuf,
    passphrase_path: PathBuf,
}

impl FileVault {
    /// Creates a vault backed by the provided encrypted file path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let passphrase_path = path.with_extension("key");
        Self {
            path,
            passphrase_path,
        }
    }

    /// Creates a vault backed by the default local MOA location.
    pub fn from_default_location() -> Result<Self> {
        let home = std::env::var("HOME").map_err(|_| MoaError::HomeDirectoryNotFound)?;
        let base = PathBuf::from(home).join(".moa");
        Ok(Self {
            path: base.join("vault.enc"),
            passphrase_path: base.join("vault.key"),
        })
    }

    /// Returns the encrypted vault file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn load_store(&self) -> Result<VaultStore> {
        if !fs::try_exists(&self.path).await? {
            return Ok(VaultStore::new());
        }

        let ciphertext = fs::read(&self.path).await?;
        let passphrase = self.ensure_passphrase().await?;
        let plaintext = decrypt_bytes(&ciphertext, passphrase)?;
        if plaintext.is_empty() {
            return Ok(VaultStore::new());
        }

        serde_json::from_slice(&plaintext).map_err(Into::into)
    }

    async fn persist_store(&self, store: &VaultStore) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| MoaError::ConfigError("vault path must have a parent".to_string()))?;
        fs::create_dir_all(parent).await?;
        let passphrase = self.ensure_passphrase().await?;
        let plaintext = serde_json::to_vec(store)?;
        let ciphertext = encrypt_bytes(&plaintext, passphrase)?;
        fs::write(&self.path, ciphertext).await?;
        #[cfg(unix)]
        fs::set_permissions(&self.path, Permissions::from_mode(0o600)).await?;
        Ok(())
    }

    async fn ensure_passphrase(&self) -> Result<SecretString> {
        if fs::try_exists(&self.passphrase_path).await? {
            let value = fs::read_to_string(&self.passphrase_path).await?;
            return Ok(SecretString::from(value.trim().to_string()));
        }

        let parent = self.passphrase_path.parent().ok_or_else(|| {
            MoaError::ConfigError("vault passphrase path must have a parent".to_string())
        })?;
        fs::create_dir_all(parent).await?;
        let generated = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        fs::write(&self.passphrase_path, &generated).await?;
        #[cfg(unix)]
        fs::set_permissions(&self.passphrase_path, Permissions::from_mode(0o600)).await?;
        debug!(path = %self.passphrase_path.display(), "generated local vault passphrase");
        Ok(SecretString::from(generated))
    }
}

#[async_trait]
impl CredentialVault for FileVault {
    async fn get(&self, service: &str, scope: &str) -> Result<Credential> {
        self.load_store()
            .await?
            .get(scope)
            .and_then(|services| services.get(service))
            .cloned()
            .ok_or_else(|| {
                MoaError::MissingEnvironmentVariable(format!(
                    "credential not configured for service {service} scope {scope}"
                ))
            })
    }

    async fn set(&self, service: &str, scope: &str, cred: Credential) -> Result<()> {
        let mut store = self.load_store().await?;
        store
            .entry(scope.to_string())
            .or_default()
            .insert(service.to_string(), cred);
        self.persist_store(&store).await
    }

    async fn delete(&self, service: &str, scope: &str) -> Result<()> {
        let mut store = self.load_store().await?;
        if let Some(services) = store.get_mut(scope) {
            services.remove(service);
            if services.is_empty() {
                store.remove(scope);
            }
        }
        self.persist_store(&store).await
    }

    async fn list(&self, scope: &str) -> Result<Vec<String>> {
        Ok(self
            .load_store()
            .await?
            .get(scope)
            .map(|services| services.keys().cloned().collect())
            .unwrap_or_default())
    }
}

fn encrypt_bytes(plaintext: &[u8], passphrase: SecretString) -> Result<Vec<u8>> {
    let encryptor = age::Encryptor::with_user_passphrase(passphrase);
    let mut ciphertext = Vec::new();
    let mut writer = encryptor.wrap_output(&mut ciphertext).map_err(|error| {
        MoaError::ProviderError(format!("failed to initialize age encryptor: {error}"))
    })?;
    writer.write_all(plaintext).map_err(|error| {
        MoaError::ProviderError(format!("failed to encrypt vault contents: {error}"))
    })?;
    writer.finish().map_err(|error| {
        MoaError::ProviderError(format!("failed to finalize vault encryption: {error}"))
    })?;
    Ok(ciphertext)
}

fn decrypt_bytes(ciphertext: &[u8], passphrase: SecretString) -> Result<Vec<u8>> {
    let decryptor = age::Decryptor::new(ciphertext).map_err(|error| {
        MoaError::ProviderError(format!("failed to initialize age decryptor: {error}"))
    })?;
    let identity = age::scrypt::Identity::new(passphrase);
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|error| {
            MoaError::ProviderError(format!("failed to decrypt vault contents: {error}"))
        })?;
    let mut plaintext = Vec::new();
    reader.read_to_end(&mut plaintext).map_err(|error| {
        MoaError::ProviderError(format!("failed to read decrypted vault contents: {error}"))
    })?;
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use moa_core::{Credential, CredentialVault};
    use tempfile::tempdir;

    use super::FileVault;

    #[tokio::test]
    async fn file_vault_encrypts_and_decrypts_roundtrip() {
        let dir = tempdir().unwrap();
        let vault = FileVault::new(dir.path().join("vault.enc"));
        let credential = Credential::Bearer("token-123".to_string());

        vault
            .set("github", "workspace-1", credential.clone())
            .await
            .unwrap();

        let raw = tokio::fs::read(vault.path()).await.unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("token-123"));

        let roundtrip = vault.get("github", "workspace-1").await.unwrap();
        assert_eq!(roundtrip, credential);
        assert_eq!(
            vault.list("workspace-1").await.unwrap(),
            vec!["github".to_string()]
        );
    }
}

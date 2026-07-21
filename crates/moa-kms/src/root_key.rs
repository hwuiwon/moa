//! Mounted deployment root-key generations used to wrap per-subject KEKs.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use zeroize::Zeroizing;

use crate::error::KmsError;

/// Length in bytes of one AES-256 deployment root key.
pub const ROOT_KEY_LEN: usize = 32;

/// The immutable root-key files mounted into one process.
///
/// Each supplied filename is the root-key generation identifier and its file
/// contents are base64 text decoding to exactly 32 bytes. The directory is read
/// by the async composition root and passed to [`Self::from_directory_entries`];
/// no ambient environment or single-file fallback exists.
pub struct RootKeyRing {
    directory: PathBuf,
    required_generation: String,
    keys: BTreeMap<String, RootKey>,
}

struct RootKey(Zeroizing<[u8; ROOT_KEY_LEN]>);

impl RootKeyRing {
    /// Decode the files read from `directory` and build an indexed keyring.
    ///
    /// `entries` contains `(filename, base64 contents)` pairs. Keeping directory
    /// I/O at the async composition root lets this crate remain runtime-agnostic
    /// while this constructor owns all key-format and generation validation.
    pub fn from_directory_entries<I, N, V>(
        directory: PathBuf,
        required_generation: impl Into<String>,
        entries: I,
    ) -> Result<Self, KmsError>
    where
        I: IntoIterator<Item = (N, V)>,
        N: Into<String>,
        V: AsRef<str>,
    {
        let required_generation = validate_generation(required_generation.into())?;
        let mut keys = BTreeMap::new();
        for (generation, encoded) in entries {
            let generation = validate_generation(generation.into())?;
            let decoded = Zeroizing::new(
                BASE64
                    .decode(encoded.as_ref().trim().as_bytes())
                    .map_err(|_| KmsError::RootKeyEncoding)?,
            );
            let material: [u8; ROOT_KEY_LEN] =
                decoded
                    .as_slice()
                    .try_into()
                    .map_err(|_| KmsError::RootKeyLength {
                        expected: ROOT_KEY_LEN,
                        actual: decoded.len(),
                    })?;
            if keys
                .insert(generation.clone(), RootKey(Zeroizing::new(material)))
                .is_some()
            {
                return Err(KmsError::RootKeyGenerationName(format!(
                    "duplicate generation {generation}"
                )));
            }
        }
        if keys.is_empty() {
            return Err(KmsError::RootKeyDirectoryEmpty(
                directory.display().to_string(),
            ));
        }
        if !keys.contains_key(&required_generation) {
            return Err(KmsError::RootKeyGenerationMissing(required_generation));
        }
        Ok(Self {
            directory,
            required_generation,
            keys,
        })
    }

    /// Directory from which this immutable keyring was loaded.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Generation this pod requires the shared database to have active.
    #[must_use]
    pub fn required_generation(&self) -> &str {
        &self.required_generation
    }

    /// Whether this process mounted `generation`.
    #[must_use]
    pub fn contains(&self, generation: &str) -> bool {
        self.keys.contains_key(generation)
    }

    /// Return the mounted generation names in deterministic order.
    pub fn generations(&self) -> impl Iterator<Item = &str> {
        self.keys.keys().map(String::as_str)
    }

    /// Borrow one root key for a single wrap or unwrap operation.
    pub(crate) fn material(&self, generation: &str) -> Result<&[u8; ROOT_KEY_LEN], KmsError> {
        self.keys
            .get(generation)
            .map(|key| &*key.0)
            .ok_or_else(|| KmsError::RootKeyGenerationMissing(generation.to_string()))
    }
}

impl fmt::Debug for RootKeyRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootKeyRing")
            .field("directory", &self.directory)
            .field("required_generation", &self.required_generation)
            .field("generations", &self.keys.keys().collect::<Vec<_>>())
            .field("material", &"<redacted>")
            .finish()
    }
}

fn validate_generation(generation: String) -> Result<String, KmsError> {
    if generation.is_empty()
        || generation == "."
        || generation == ".."
        || generation.contains('/')
        || generation.contains('\\')
    {
        return Err(KmsError::RootKeyGenerationName(generation));
    }
    Ok(generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(seed: u8) -> String {
        BASE64.encode([seed; ROOT_KEY_LEN])
    }

    #[test]
    fn directory_entries_are_indexed_by_filename_offline() {
        // Pins: filenames are generation IDs, values are base64-only 32-byte keys,
        // and the configured required generation must be present.
        let ring = RootKeyRing::from_directory_entries(
            PathBuf::from("/keys"),
            "g2",
            [("g1", encoded(1)), ("g2", encoded(2))],
        )
        .expect("build ring");

        assert_eq!(ring.directory(), Path::new("/keys"));
        assert_eq!(ring.required_generation(), "g2");
        assert_eq!(ring.generations().collect::<Vec<_>>(), vec!["g1", "g2"]);
        assert_eq!(ring.material("g1").expect("g1"), &[1; ROOT_KEY_LEN]);
    }

    #[test]
    fn missing_required_generation_is_rejected_offline() {
        // Pins: a pod cannot start with a required generation absent from its mount.
        let error =
            RootKeyRing::from_directory_entries(PathBuf::from("/keys"), "g2", [("g1", encoded(1))])
                .expect_err("missing required generation must fail");
        assert!(matches!(
            error,
            KmsError::RootKeyGenerationMissing(generation) if generation == "g2"
        ));
    }

    #[test]
    fn raw_or_wrong_length_material_is_rejected_offline() {
        // Pins: files accept only base64 that decodes to exactly 32 bytes.
        let bad_base64 = RootKeyRing::from_directory_entries(
            PathBuf::from("/keys"),
            "g1",
            [("g1", "not base64")],
        )
        .expect_err("raw text must fail");
        assert!(matches!(bad_base64, KmsError::RootKeyEncoding));

        let short = RootKeyRing::from_directory_entries(
            PathBuf::from("/keys"),
            "g1",
            [("g1", BASE64.encode([1_u8; 16]))],
        )
        .expect_err("short key must fail");
        assert!(matches!(
            short,
            KmsError::RootKeyLength {
                expected: 32,
                actual: 16
            }
        ));
    }

    #[test]
    fn debug_never_reveals_material_offline() {
        // Pins: diagnostics show generations but never key bytes.
        let ring =
            RootKeyRing::from_directory_entries(PathBuf::from("/keys"), "g1", [("g1", encoded(9))])
                .expect("ring");
        let rendered = format!("{ring:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("[9"), "material leaked: {rendered}");
    }
}

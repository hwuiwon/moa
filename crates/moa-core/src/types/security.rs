//! Shared sensitivity vocabulary for storage, retrieval, and outbound egress.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::MoaError;

/// Sensitivity class attached to data throughout MOA.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SensitivityClass {
    /// No sensitive data is known to be present.
    #[default]
    None,
    /// Personally identifiable information.
    Pii,
    /// Protected health information.
    Phi,
    /// Restricted data requiring explicit policy handling.
    Restricted,
}

impl SensitivityClass {
    /// Returns the canonical lowercase representation used by SQL and providers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pii => "pii",
            Self::Phi => "phi",
            Self::Restricted => "restricted",
        }
    }

    /// Returns the stable ordering rank used by sensitivity ceilings.
    #[must_use]
    pub const fn rank(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Pii => 1,
            Self::Phi => 2,
            Self::Restricted => 3,
        }
    }
}

impl std::fmt::Display for SensitivityClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SensitivityClass {
    type Err = MoaError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "pii" => Ok(Self::Pii),
            "phi" => Ok(Self::Phi),
            "restricted" => Ok(Self::Restricted),
            other => Err(MoaError::ConfigError(format!(
                "unknown sensitivity class '{other}'"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::SensitivityClass;

    #[test]
    fn sensitivity_class_has_one_canonical_string_and_rank() {
        // Pins: SQL, graph, vector, classifier, and MCP policy code share exactly
        // one ordered none/pii/phi/restricted vocabulary.
        let expected = [
            (SensitivityClass::None, "none", 0),
            (SensitivityClass::Pii, "pii", 1),
            (SensitivityClass::Phi, "phi", 2),
            (SensitivityClass::Restricted, "restricted", 3),
        ];

        for (class, name, rank) in expected {
            assert_eq!(class.as_str(), name);
            assert_eq!(class.rank(), rank);
            assert_eq!(
                SensitivityClass::from_str(name).expect("canonical class"),
                class
            );
        }
    }
}

//! Offline contracts for the closed managed knowledge-parent definitions.

use moa_artifacts::connector::RuntimeConnectorAuthRequirement;
use moa_connectors::Error;
use moa_connectors::domain::{ConnectionDefinitionRef, ManagedParentDefinition};
use moa_core::types::credentials::CredentialSlotName;

#[test]
fn managed_knowledge_parent_definitions_are_closed_and_exact_offline() {
    // Pins: arbitrary provider labels cannot acquire a code-owned connection,
    // while the two supported providers resolve to their exact immutable refs.
    assert_eq!(
        ManagedParentDefinition::for_knowledge_provider("nango")
            .expect("Nango is a closed managed provider")
            .definition_ref(),
        ConnectionDefinitionRef::built_in("knowledge:nango", 1)
            .expect("fixture built-in reference should be valid")
    );
    assert_eq!(
        ManagedParentDefinition::for_knowledge_provider("merge")
            .expect("Merge is a closed managed provider")
            .definition_ref(),
        ConnectionDefinitionRef::built_in("knowledge:merge", 1)
            .expect("fixture built-in reference should be valid")
    );
    assert!(matches!(
        ManagedParentDefinition::for_knowledge_provider("custom-api"),
        Err(Error::UnsupportedManagedKnowledgeProvider)
    ));
}

#[test]
fn managed_knowledge_parents_pin_provider_specific_auth_offline() {
    // Pins: Nango uses only its deployment-owned provider handle, while Merge
    // cannot activate until the tenant's primary bearer credential is ready.
    let nango = ManagedParentDefinition::KnowledgeNango.credential_requirements();
    assert_eq!(nango, vec![RuntimeConnectorAuthRequirement::None]);

    let merge = ManagedParentDefinition::KnowledgeMerge.credential_requirements();
    assert_eq!(
        merge,
        vec![RuntimeConnectorAuthRequirement::Bearer {
            slot: CredentialSlotName::PRIMARY,
        }]
    );
}

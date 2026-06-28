//! Adversarial out-of-line tests for BLAKE3 Merkle roots and inclusion proofs.

#[path = "support/fixture_json.rs"]
mod support;

use moa_lineage_audit::{
    AuditError, blake3_inclusion_proof, blake3_merkle_root, verify_blake3_inclusion,
};
use serde::Deserialize;
use support::fixture_json;

#[derive(Debug, Deserialize)]
struct Blake3Vectors {
    cases: Vec<Blake3Case>,
}

#[derive(Debug, Deserialize)]
struct Blake3Case {
    name: String,
    #[serde(default)]
    leaves_hex: Vec<String>,
    #[serde(default)]
    leaves_utf8: Vec<String>,
    expected_root_hex: String,
}

// These fixtures pin MOA's own domain-separated construction (0x00 leaf / 0x01
// node prefixes) rather than an external KAT, so they are regression vectors:
// they fail loudly if the construction changes, but they are self-generated.

#[test]
fn merkle_root_matches_blake3_regression_vector_for_single_leaf() {
    assert_merkle_root_matches_fixture("single_empty");
}

#[test]
fn merkle_root_matches_blake3_regression_vector_for_balanced_tree_of_4_leaves() {
    assert_merkle_root_matches_fixture("balanced_4");
}

#[test]
fn merkle_root_matches_blake3_regression_vector_for_unbalanced_tree_of_5_leaves() {
    assert_merkle_root_matches_fixture("unbalanced_5");
}

#[test]
fn merkle_proof_verifies_for_each_leaf_in_a_tree_of_8_leaves() {
    let leaves = (0..8)
        .map(|idx| format!("record-{idx}").into_bytes())
        .collect::<Vec<_>>();
    let root = blake3_merkle_root(&leaves).expect("root should compute");

    for (index, leaf) in leaves.iter().enumerate() {
        let proof = blake3_inclusion_proof(&leaves, index).expect("proof should compute");
        verify_blake3_inclusion(leaf, index, &proof, root)
            .unwrap_or_else(|error| panic!("proof for leaf {index} should verify: {error}"));
    }
}

/// Tampering: verifies the proof generated for leaf 3 against the bytes of leaf 4.
#[test]
fn merkle_proof_fails_when_leaf_is_swapped_for_a_different_leaf() {
    let leaves = (0..8)
        .map(|idx| format!("record-{idx}").into_bytes())
        .collect::<Vec<_>>();
    let root = blake3_merkle_root(&leaves).expect("root should compute");
    let proof = blake3_inclusion_proof(&leaves, 3).expect("proof should compute");

    let error = verify_blake3_inclusion(&leaves[4], 3, &proof, root)
        .expect_err("swapped leaf should fail inclusion proof verification");

    assert!(
        matches!(error, AuditError::Invalid(ref message) if message.contains("inclusion proof")),
        "expected inclusion-proof invalid error, got {error:?}"
    );
}

/// Tampering: a single flipped byte in a proof sibling must break verification.
#[test]
fn merkle_proof_fails_when_a_sibling_hash_is_mutated() {
    let leaves = (0..8)
        .map(|idx| format!("record-{idx}").into_bytes())
        .collect::<Vec<_>>();
    let root = blake3_merkle_root(&leaves).expect("root should compute");
    let mut proof = blake3_inclusion_proof(&leaves, 3).expect("proof should compute");

    // Flip one bit in the first sibling hash; the recomputed root must diverge.
    proof[0][0] ^= 0x01;

    let error = verify_blake3_inclusion(&leaves[3], 3, &proof, root)
        .expect_err("a mutated proof sibling should fail inclusion verification");

    assert!(
        matches!(error, AuditError::Invalid(ref message) if message.contains("inclusion proof")),
        "expected inclusion-proof invalid error, got {error:?}"
    );
}

/// Tampering: a correct leaf and proof verified at the wrong index must fail,
/// because the left/right combination order at each level flips with parity.
#[test]
fn merkle_proof_fails_when_verified_at_the_wrong_index() {
    let leaves = (0..8)
        .map(|idx| format!("record-{idx}").into_bytes())
        .collect::<Vec<_>>();
    let root = blake3_merkle_root(&leaves).expect("root should compute");
    let proof = blake3_inclusion_proof(&leaves, 3).expect("proof should compute");

    // Leaf 3 sits at an odd index; verifying it as the even index 2 reverses the
    // first node-hash combination order and cannot reproduce the root.
    let error = verify_blake3_inclusion(&leaves[3], 2, &proof, root)
        .expect_err("verifying a leaf at the wrong index should fail");

    assert!(
        matches!(error, AuditError::Invalid(ref message) if message.contains("inclusion proof")),
        "expected inclusion-proof invalid error, got {error:?}"
    );
}

/// Tampering: a valid leaf and proof checked against a different tree's root
/// must fail, so a substituted root cannot launder an inclusion claim.
#[test]
fn merkle_proof_fails_against_a_different_trees_root() {
    let leaves = (0..8)
        .map(|idx| format!("record-{idx}").into_bytes())
        .collect::<Vec<_>>();
    let proof = blake3_inclusion_proof(&leaves, 3).expect("proof should compute");

    let other_leaves = (0..8)
        .map(|idx| format!("forged-{idx}").into_bytes())
        .collect::<Vec<_>>();
    let wrong_root = blake3_merkle_root(&other_leaves).expect("wrong root should compute");

    let error = verify_blake3_inclusion(&leaves[3], 3, &proof, wrong_root)
        .expect_err("a proof checked against the wrong root should fail");

    assert!(
        matches!(error, AuditError::Invalid(ref message) if message.contains("inclusion proof")),
        "expected inclusion-proof invalid error, got {error:?}"
    );
}

fn assert_merkle_root_matches_fixture(case_name: &str) {
    let case = vector_case(case_name);
    let leaves = leaves_for_case(&case);
    let root = blake3_merkle_root(&leaves).expect("root should compute");
    let expected = hex::decode(&case.expected_root_hex).expect("expected root should be hex");

    assert_eq!(root.as_bytes().as_slice(), expected.as_slice());
}

fn vector_case(case_name: &str) -> Blake3Case {
    let vectors: Blake3Vectors = serde_json::from_value(fixture_json("blake3_known_vectors.json"))
        .expect("BLAKE3 vector fixture should parse");
    vectors
        .cases
        .into_iter()
        .find(|case| case.name == case_name)
        .unwrap_or_else(|| panic!("missing BLAKE3 vector case {case_name}"))
}

fn leaves_for_case(case: &Blake3Case) -> Vec<Vec<u8>> {
    if !case.leaves_hex.is_empty() {
        return case
            .leaves_hex
            .iter()
            .map(|leaf| hex::decode(leaf).expect("leaf should decode from hex"))
            .collect();
    }
    case.leaves_utf8
        .iter()
        .map(|leaf| leaf.as_bytes().to_vec())
        .collect()
}

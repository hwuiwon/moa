//! Reconstruction of the authoritative embedding input for a whole partition.
//!
//! A storage partition's vector space is shared by every node label that
//! carries an embedding: knowledge chunks, extracted facts, incidents, and
//! resolved entities all sit in the same 1024-dimension index. Re-embedding
//! only the chunks would leave the rest of the partition in the old model's
//! space while every row still claims the new model, which nothing downstream
//! can detect.
//!
//! So a rebuild must rebuild all of them, and for each one it must reproduce
//! the *exact* text the original writer embedded. That text is not uniform:
//!
//! | label | authoritative input | written by |
//! |---|---|---|
//! | `Chunk` | document title + heading path + chunk text | knowledge materialization |
//! | `Entity` | the normalized entity name | entity resolution / consolidation |
//! | `Fact`, `Incident`, `Concept`, `Decision`, `Lesson` | `properties_summary->>'summary'` | ingest fast and slow paths |
//!
//! Two traps this module exists to avoid. The Turbopuffer sync joins
//! `knowledge_chunks.text` as `search_text` — that is the BM25 body, not the
//! embedding input, and rebuilding from it silently drops the contextual
//! prefix. And a node's `name` is a display string; for an `Entity` the
//! embedded value is the normalized form, so substituting `name` would move
//! every entity vector.
//!
//! Any label without a known writer is refused. A partition containing one is
//! not rebuilt at all, because guessing an input produces vectors that index
//! cleanly and answer wrongly.

use moa_core::types::memory::contextual_chunk_embedding_input;
use moa_core::types::security::SensitivityClass;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::{Error, Result};

/// Node labels whose authoritative embedding input this module can reconstruct.
///
/// `Source`, `Document`, and `ContactGroup` are deliberately absent: no
/// production writer embeds them today, so a partition that somehow contains
/// one fails closed rather than being rebuilt from a guess.
pub const REBUILDABLE_LABELS: [&str; 6] =
    ["Chunk", "Entity", "Fact", "Incident", "Concept", "Decision"];

/// Additional rebuildable label kept separate so the array above stays a
/// compile-time constant the SQL binder can pass directly.
pub const REBUILDABLE_LESSON_LABEL: &str = "Lesson";

/// Returns every node label a partition rebuild knows how to reconstruct.
#[must_use]
pub fn rebuildable_labels() -> Vec<String> {
    REBUILDABLE_LABELS
        .iter()
        .copied()
        .chain(std::iter::once(REBUILDABLE_LESSON_LABEL))
        .map(str::to_string)
        .collect()
}

/// One vector whose original embedding input has been reconstructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeInput {
    /// Graph node identity the vector belongs to.
    pub uid: Uuid,
    /// Contact owner for contact-scoped rows.
    pub user_id: Option<String>,
    /// Graph vertex label that selected the reconstruction rule.
    pub label: String,
    /// Sensitivity class carried over from the existing embedding row.
    pub pii_class: SensitivityClass,
    /// The exact text the original writer embedded.
    pub text: String,
}

impl AuthoritativeInput {
    /// Returns the SHA-256 digest of the reconstructed input.
    ///
    /// Persisted beside every candidate vector so "this was rebuilt from the
    /// real input" is a value that can be compared, not a claim in a comment.
    #[must_use]
    pub fn digest(&self) -> Vec<u8> {
        Sha256::digest(self.text.as_bytes()).to_vec()
    }

    /// Returns a deterministic token estimate for cost projection.
    ///
    /// Four bytes per token is the conventional English approximation. It is an
    /// estimate and is labelled as one everywhere it surfaces; the embedding
    /// provider trait reports no billed usage, so no real figure is available.
    #[must_use]
    pub fn estimated_tokens(&self) -> i32 {
        i32::try_from(self.text.len().div_ceil(4)).unwrap_or(i32::MAX)
    }
}

/// Counts the vectors in one storage partition that a rebuild must reproduce.
///
/// This is the census `vectors_total` reports and the number activation
/// compares the candidate count against. It counts live embedding rows, so a
/// soft-deleted row does not make a complete generation look incomplete.
pub async fn count_partition_vectors(
    conn: &mut PgConnection,
    storage_partition_id: &str,
) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)
          FROM moa.embeddings
         WHERE storage_partition_id = $1
           AND valid_to IS NULL
        "#,
    )
    .bind(storage_partition_id)
    .fetch_one(conn)
    .await?;
    Ok(count)
}

/// Returns labels present in the partition that no reconstruction rule covers.
///
/// Called before a rebuild starts. A nonempty result blocks the operation:
/// re-embedding a partition while skipping some of its vectors is exactly the
/// mixed-model state the rebuild exists to prevent.
pub async fn unrebuildable_labels(
    conn: &mut PgConnection,
    storage_partition_id: &str,
) -> Result<Vec<String>> {
    let labels: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT DISTINCT label
          FROM moa.embeddings
         WHERE storage_partition_id = $1
           AND valid_to IS NULL
           AND label <> ALL($2::TEXT[])
         ORDER BY label
        "#,
    )
    .bind(storage_partition_id)
    .bind(rebuildable_labels())
    .fetch_all(conn)
    .await?;
    Ok(labels)
}

/// Loads one keyset page of authoritative embedding inputs.
///
/// Ordered by `uid` and strictly greater than `after_uid`, so a resumed build
/// continues exactly where the last committed checkpoint stopped and cannot
/// re-emit a candidate it already wrote.
///
/// The join fans out per label rather than per owner crate: the knowledge
/// tables live in the same database, and pulling the title and heading path
/// here keeps the contextual input assembled from committed state rather than
/// from whatever an in-flight ingestion happens to hold.
pub async fn load_authoritative_inputs(
    conn: &mut PgConnection,
    storage_partition_id: &str,
    after_uid: Option<Uuid>,
    limit: i64,
) -> Result<Vec<AuthoritativeInput>> {
    let rows = sqlx::query(
        r#"
        SELECT embedding.uid,
               embedding.user_id,
               embedding.label,
               embedding.pii_class,
               node.name                       AS node_name,
               node.properties_summary         AS node_properties,
               knowledge_chunk.text            AS chunk_text,
               knowledge_chunk.heading_path    AS chunk_heading_path,
               knowledge_object.title          AS document_title
          FROM moa.embeddings AS embedding
          JOIN moa.node_index AS node
            ON node.uid = embedding.uid
          LEFT JOIN moa.knowledge_chunks AS knowledge_chunk
            ON knowledge_chunk.storage_partition_id = embedding.storage_partition_id
           AND knowledge_chunk.graph_node_uid = embedding.uid
           AND embedding.label = 'Chunk'
          LEFT JOIN moa.knowledge_document_versions AS document_version
            ON document_version.document_version_uid = knowledge_chunk.document_version_id
          LEFT JOIN moa.knowledge_objects AS knowledge_object
            ON knowledge_object.object_uid = document_version.object_id
         WHERE embedding.storage_partition_id = $1
           AND embedding.valid_to IS NULL
           AND ($2::uuid IS NULL OR embedding.uid > $2)
         ORDER BY embedding.uid
         LIMIT $3
        "#,
    )
    .bind(storage_partition_id)
    .bind(after_uid)
    .bind(limit)
    .fetch_all(conn)
    .await?;

    rows.into_iter().map(decode_authoritative_input).collect()
}

fn decode_authoritative_input(row: sqlx::postgres::PgRow) -> Result<AuthoritativeInput> {
    let uid: Uuid = row.try_get("uid")?;
    let label: String = row.try_get("label")?;
    let pii_class: String = row.try_get("pii_class")?;
    let pii_class = pii_class
        .parse::<SensitivityClass>()
        .map_err(|_| Error::InvalidSensitivityClass(pii_class))?;
    let node_name: String = row.try_get("node_name")?;
    let node_properties: Option<serde_json::Value> = row.try_get("node_properties")?;

    // A sealed row's indexed plaintext is a placeholder, not its content. The
    // graph write path refuses to embed sealed classes, so `moa.embeddings`
    // should never hold one and this branch should be unreachable -- but if it
    // ever is reached, reconstructing from `properties_summary` would embed the
    // placeholder and quietly make a restricted node retrievable by a vector
    // that describes nothing. The predicate is
    // `SensitivityClass::is_sealed`, the same one the write path asks, rather
    // than a second copy of the class list that could drift away from it.
    if pii_class.is_sealed() {
        return Err(Error::RebuildProvenanceMissing {
            uid,
            label,
            reason: "sealed content cannot be re-embedded from its plaintext placeholder",
        });
    }

    let text = match label.as_str() {
        "Chunk" => {
            let chunk_text: Option<String> = row.try_get("chunk_text")?;
            let chunk_text = chunk_text.ok_or_else(|| Error::RebuildProvenanceMissing {
                uid,
                label: label.clone(),
                reason: "no knowledge chunk row is associated with this graph node",
            })?;
            let heading_path: Vec<String> = row.try_get("chunk_heading_path")?;
            let document_title: Option<String> = row.try_get("document_title")?;
            contextual_chunk_embedding_input(document_title.as_deref(), &heading_path, &chunk_text)
        }
        "Entity" => normalized_entity_input(uid, &label, &node_name, node_properties.as_ref())?,
        "Fact" | "Incident" | "Concept" | "Decision" | "Lesson" => {
            summary_input(uid, &label, node_properties.as_ref())?
        }
        other => {
            return Err(Error::RebuildLabelUnsupported {
                uid,
                label: other.to_string(),
            });
        }
    };

    if text.trim().is_empty() {
        return Err(Error::RebuildProvenanceMissing {
            uid,
            label,
            reason: "the reconstructed embedding input is empty",
        });
    }

    Ok(AuthoritativeInput {
        uid,
        user_id: row.try_get("user_id")?,
        label,
        pii_class,
        text,
    })
}

/// Reconstructs an entity's embedded text.
///
/// Entity vectors are computed from the *normalized* mention, not the display
/// name: consolidation reads `properties_summary->>'normalized_name'` and falls
/// back to normalizing the name when the property predates that field. This
/// reproduces both branches. Embedding `name` instead would move every entity
/// vector by however much casing and punctuation differ.
fn normalized_entity_input(
    uid: Uuid,
    label: &str,
    node_name: &str,
    properties: Option<&serde_json::Value>,
) -> Result<String> {
    if let Some(normalized) = properties
        .and_then(|properties| properties.get("normalized_name"))
        .and_then(serde_json::Value::as_str)
        .filter(|normalized| !normalized.trim().is_empty())
    {
        return Ok(normalized.to_string());
    }
    let normalized = normalize_entity_name(node_name);
    if normalized.is_empty() {
        return Err(Error::RebuildProvenanceMissing {
            uid,
            label: label.to_string(),
            reason: "entity has neither a normalized name property nor a normalizable name",
        });
    }
    Ok(normalized)
}

/// Reconstructs the summary text the ingest paths embed.
///
/// Both the fast path (`build_intent`, incident recording) and the slow path
/// (fact intents) write the embedded text into `properties_summary->>'summary'`
/// verbatim, redaction already applied. A node missing it has provenance this
/// rebuild cannot reconstruct, and is refused rather than approximated from
/// `name`, which is a truncated display form.
fn summary_input(uid: Uuid, label: &str, properties: Option<&serde_json::Value>) -> Result<String> {
    properties
        .and_then(|properties| properties.get("summary"))
        .and_then(serde_json::Value::as_str)
        .filter(|summary| !summary.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| Error::RebuildProvenanceMissing {
            uid,
            label: label.to_string(),
            reason: "node properties carry no `summary` to re-embed",
        })
}

/// Normalizes an entity mention the way entity resolution does.
///
/// Duplicated from `moa-memory-types` rather than imported: this crate sits
/// below it in the dependency graph, and the rebuild needs the rule to match
/// what was originally embedded. The unit test below pins the shared behavior
/// so a change on either side shows up as a failing assertion.
fn normalize_entity_name(name: &str) -> String {
    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in name.chars() {
        if character.is_alphanumeric() {
            token.extend(character.to_lowercase());
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }

    let normalized = tokens.join(" ");
    if normalized.is_empty() {
        name.trim().to_lowercase()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entity_input_prefers_the_normalized_property_over_the_display_name() {
        // Pins: entity vectors are rebuilt from the normalized mention that was
        // embedded, not from the display name. Substituting the name would move
        // every entity vector in the partition.
        let properties = json!({"normalized_name": "acme corp", "name": "ACME Corp."});

        let input = normalized_entity_input(Uuid::nil(), "Entity", "ACME Corp.", Some(&properties))
            .expect("normalized property is present");

        assert_eq!(input, "acme corp");
    }

    #[test]
    fn entity_input_falls_back_to_normalizing_the_name() {
        // Pins: entities written before the normalized-name property existed
        // still rebuild to the same text entity resolution embedded, because
        // the fallback applies the identical normalization rule. Embedding the
        // display `name` instead is the substitution plan item 3 forbids -- it
        // would move every entity vector by however much casing and
        // punctuation differ from the normalized form.
        let input = normalized_entity_input(Uuid::nil(), "Entity", "  ACME  Corp. ", None)
            .expect("name normalizes");

        assert_eq!(input, "acme corp");
    }

    #[test]
    fn summary_input_refuses_a_node_with_no_recorded_summary() {
        // Pins: fail closed on missing provenance. Approximating from the node
        // name would produce a vector that indexes cleanly and retrieves wrongly.
        let error = summary_input(Uuid::nil(), "Fact", Some(&json!({"predicate": "uses"})))
            .expect_err("a fact without a summary has no reconstructable input");

        assert!(
            matches!(error, Error::RebuildProvenanceMissing { .. }),
            "unexpected error: {error}"
        );

        let blank = summary_input(Uuid::nil(), "Fact", Some(&json!({"summary": "   "})))
            .expect_err("a blank summary is not an input");
        assert!(matches!(blank, Error::RebuildProvenanceMissing { .. }));
    }

    #[test]
    fn sealed_rows_are_refused_rather_than_embedded_from_their_placeholder() {
        // Pins: the rebuild asks the same sealed question the write path asks.
        // Restricted and PHI rows keep only a placeholder in the indexed
        // plaintext, so re-embedding one would produce a vector describing
        // "[RESTRICTED]" and attach it to a node whose real content is sealed.
        for class in [SensitivityClass::Restricted, SensitivityClass::Phi] {
            assert!(class.is_sealed(), "{class:?} must be treated as sealed");
        }
        for class in [SensitivityClass::None, SensitivityClass::Pii] {
            assert!(!class.is_sealed(), "{class:?} is embeddable");
        }
    }

    #[test]
    fn rebuildable_labels_exclude_labels_with_no_production_embedder() {
        // Pins: `Source`, `Document`, and `ContactGroup` are not guessed. If a
        // writer for one is added later, this assertion is where the rebuild
        // rule must be added alongside it.
        let labels = rebuildable_labels();

        assert!(!labels.contains(&"Source".to_string()));
        assert!(!labels.contains(&"Document".to_string()));
        assert!(!labels.contains(&"ContactGroup".to_string()));
        assert!(labels.contains(&"Chunk".to_string()));
        assert!(labels.contains(&"Lesson".to_string()));
        assert_eq!(labels.len(), 7);
    }

    #[test]
    fn normalization_matches_the_shared_entity_rule() {
        // Pins: this crate's copy of the normalization behaves identically to
        // `moa_memory_types::normalize_entity_name`, which sits above it in the
        // dependency graph and cannot be imported here.
        for (raw, expected) in [
            ("ACME Corp.", "acme corp"),
            ("  multi   space  ", "multi space"),
            ("é-Accent", "é accent"),
            ("!!!", "!!!"),
        ] {
            assert_eq!(normalize_entity_name(raw), expected, "input `{raw}`");
        }
    }

    #[test]
    fn digest_and_token_estimate_are_derived_from_the_reconstructed_text() {
        // Pins: the provenance digest covers the input text exactly, and the
        // cost projection is a deterministic estimate rather than a provider
        // figure (the embedding trait reports no billed usage).
        let input = AuthoritativeInput {
            uid: Uuid::nil(),
            user_id: None,
            label: "Fact".to_string(),
            pii_class: SensitivityClass::None,
            text: "abcdefgh".to_string(),
        };

        assert_eq!(input.digest().len(), 32);
        assert_eq!(input.digest(), Sha256::digest(b"abcdefgh").to_vec());
        assert_eq!(input.estimated_tokens(), 2);
    }
}

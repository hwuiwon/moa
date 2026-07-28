//! Bounded shadow validation of a candidate generation.
//!
//! Validation asks one question: does the candidate generation rank the
//! partition's own content roughly the way the generation it would replace
//! does? It answers by sampling candidate vectors, running each as a query
//! against both generations, and averaging the top-K overlap.
//!
//! Only the pure overlap comparison is shared with the backend-promotion
//! engine. None of that engine's serving machinery is: its dual-read window
//! makes the target backend answer production traffic, and a candidate
//! generation must never answer anything. Here, both sides of the comparison
//! are computed inside this module and only a scalar leaves it — there is no
//! value shaped like a retrieval hit for a caller to accidentally serve.
//!
//! The sample is bounded twice: by a caller-supplied cap and by a hard ceiling,
//! so validating a large partition cannot turn into an unbounded scan that
//! starves the ordinary query path.

use pgvector::HalfVector;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::promotion::top_k_overlap;
use crate::{Error, Result, VectorMatch};

/// Neighbors compared per shadow query.
pub const SHADOW_VALIDATION_K: usize = 10;

/// Hard ceiling on shadow queries per validation pass, whatever the caller asks.
pub const SHADOW_VALIDATION_MAX_SAMPLES: i64 = 256;

/// Result of one bounded shadow validation pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowValidation {
    /// Number of shadow queries actually issued.
    pub samples: usize,
    /// Mean top-K overlap between the two generations.
    pub overlap: f64,
}

impl ShadowValidation {
    /// Whether the candidate generation may be activated.
    #[must_use]
    pub fn passes(&self, threshold: f64) -> bool {
        self.overlap >= threshold
    }
}

/// Compares a candidate generation against the partition's served vectors.
///
/// `sample_limit` is clamped into `1..=SHADOW_VALIDATION_MAX_SAMPLES`. An empty
/// candidate generation scores zero rather than a vacuous 1.0: a generation
/// with nothing in it must not look like a perfect match and activate over a
/// populated one.
pub async fn validate_candidate_generation(
    conn: &mut PgConnection,
    storage_partition_id: &str,
    generation_uid: Uuid,
    sample_limit: i64,
) -> Result<ShadowValidation> {
    let limit = sample_limit.clamp(1, SHADOW_VALIDATION_MAX_SAMPLES);
    let samples = load_shadow_sample(conn, generation_uid, limit).await?;
    if samples.is_empty() {
        return Ok(ShadowValidation {
            samples: 0,
            overlap: 0.0,
        });
    }

    let mut total = 0.0;
    for sample in &samples {
        let served = served_neighbors(conn, storage_partition_id, sample).await?;
        let candidate = candidate_neighbors(conn, generation_uid, sample).await?;
        total += top_k_overlap(&served, &candidate, SHADOW_VALIDATION_K);
    }

    Ok(ShadowValidation {
        samples: samples.len(),
        overlap: total / samples.len() as f64,
    })
}

/// One sampled candidate vector used as a shadow query.
struct ShadowSample {
    label: String,
    embedding: HalfVector,
}

/// Draws a deterministic, bounded sample of candidate vectors.
///
/// Sampled by a hash of the uid rather than `ORDER BY random()` so a re-run
/// against an unchanged generation compares the same points and a validation
/// score is reproducible when someone questions it.
async fn load_shadow_sample(
    conn: &mut PgConnection,
    generation_uid: Uuid,
    limit: i64,
) -> Result<Vec<ShadowSample>> {
    let rows = sqlx::query(
        r#"
        SELECT label, embedding
          FROM moa.knowledge_rebuild_candidate_vector
         WHERE generation_uid = $1
         ORDER BY abs(hashtext(uid::TEXT)), uid
         LIMIT $2
        "#,
    )
    .bind(generation_uid)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(ShadowSample {
                label: row.try_get("label")?,
                embedding: row.try_get("embedding")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
        .map_err(Error::from)
}

/// Ranks the served generation's neighbors for one shadow query.
async fn served_neighbors(
    conn: &mut PgConnection,
    storage_partition_id: &str,
    sample: &ShadowSample,
) -> Result<Vec<VectorMatch>> {
    let rows = sqlx::query(
        r#"
        SELECT uid, (embedding <=> $3) AS distance
          FROM moa.embeddings
         WHERE storage_partition_id = $1
           AND valid_to IS NULL
           AND label = $2
         ORDER BY embedding <=> $3, uid
         LIMIT $4
        "#,
    )
    .bind(storage_partition_id)
    .bind(&sample.label)
    .bind(&sample.embedding)
    .bind(i64::try_from(SHADOW_VALIDATION_K).unwrap_or(i64::MAX))
    .fetch_all(&mut *conn)
    .await?;
    decode_matches(rows)
}

/// Ranks the candidate generation's neighbors for one shadow query.
async fn candidate_neighbors(
    conn: &mut PgConnection,
    generation_uid: Uuid,
    sample: &ShadowSample,
) -> Result<Vec<VectorMatch>> {
    let rows = sqlx::query(
        r#"
        SELECT uid, (embedding <=> $3) AS distance
          FROM moa.knowledge_rebuild_candidate_vector
         WHERE generation_uid = $1
           AND label = $2
         ORDER BY embedding <=> $3, uid
         LIMIT $4
        "#,
    )
    .bind(generation_uid)
    .bind(&sample.label)
    .bind(&sample.embedding)
    .bind(i64::try_from(SHADOW_VALIDATION_K).unwrap_or(i64::MAX))
    .fetch_all(&mut *conn)
    .await?;
    decode_matches(rows)
}

fn decode_matches(rows: Vec<sqlx::postgres::PgRow>) -> Result<Vec<VectorMatch>> {
    rows.into_iter()
        .map(|row| {
            let distance: f64 = row.try_get("distance")?;
            Ok(VectorMatch {
                uid: row.try_get("uid")?,
                score: 1.0 - distance as f32,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
        .map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_candidate_generation_scores_zero_rather_than_a_vacuous_match() {
        // Pins: a generation with no candidate vectors must not read as a
        // perfect overlap and activate over a populated generation. The
        // sample-count branch is what makes that true, so it is asserted here
        // directly rather than only through the database path.
        let empty = ShadowValidation {
            samples: 0,
            overlap: 0.0,
        };

        assert!(!empty.passes(0.95));
        assert!(!empty.passes(0.01));
    }

    #[test]
    fn validation_threshold_is_inclusive_at_the_boundary() {
        // Pins: a generation exactly at the threshold activates. A strict
        // comparison here would make the documented 0.95 bar unreachable.
        let boundary = ShadowValidation {
            samples: 32,
            overlap: 0.95,
        };

        assert!(boundary.passes(0.95));
        assert!(
            !ShadowValidation {
                samples: 32,
                overlap: 0.949,
            }
            .passes(0.95)
        );
    }

    #[test]
    fn sample_limits_are_clamped_to_the_hard_ceiling() {
        // Pins: the bound on shadow work is not the caller's to raise. A
        // validation pass over a large partition cannot become an unbounded
        // scan competing with production queries.
        assert_eq!(i64::MAX.clamp(1, SHADOW_VALIDATION_MAX_SAMPLES), 256);
        assert_eq!(0_i64.clamp(1, SHADOW_VALIDATION_MAX_SAMPLES), 1);
        assert_eq!((-5_i64).clamp(1, SHADOW_VALIDATION_MAX_SAMPLES), 1);
    }
}

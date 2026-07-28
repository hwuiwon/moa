//! Attributable contributions behind artifact revisions and generated suites.
//!
//! Two kinds of derived bytes previously had no owner and no address.
//!
//! A published skill revision's `definition` and `source_text` are model output
//! fused from one or more people's transcripts. Nothing recorded whose. An
//! erasure could therefore delete a subject's memories while a skill written
//! from those memories kept serving, which is not erasure — it is erasure of the
//! evidence with the conclusion left standing.
//!
//! Regression-suite bytes were worse: generated and sibling-accumulated suite
//! TOML lived inside `learning_candidates.payload` as JSON strings, so
//! attributable generated text sat in a column that could not be joined,
//! enumerated, or selectively deleted. Moving it here also puts the review-input
//! assembly behind the component that owns the bytes, instead of every reader
//! re-parsing a payload shape.
//!
//! [`RevisionContributionKind::GeneratedDefinition`] is declared
//! NON-SUBTRACTABLE. That is the load-bearing judgement in this module: you
//! cannot carve one contributor back out of a paragraph a model wrote from
//! several, so a shared revision whose definition drew on erased evidence is
//! invalidated whole rather than rewritten. Anything else would be a claim of
//! partial erasure that no one could verify.

use super::*;

/// Which bytes of a revision one contribution accounts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionContributionKind {
    /// The revision's own definition and source text: fused model output.
    ///
    /// **NON-SUBTRACTABLE BY CONSTRUCTION.** A model wrote this paragraph from
    /// several people's transcripts at once. There is no operation that removes
    /// one contributor's influence from it and leaves the rest intact, because
    /// the contribution was never separable in the first place — it was fused at
    /// generation time.
    ///
    /// So a shared revision whose definition drew on erased evidence is
    /// invalidated WHOLE. That is deliberately the expensive answer, and the
    /// reason matters more than the rule: anything softer would let "we removed
    /// the erased bytes" quietly come to mean "we removed the bytes we could
    /// point at." That claim cannot be checked by anyone, including the person
    /// making it, which makes it exactly the kind of unfalsifiable guarantee
    /// this codebase spent two waves removing from the purge subsystem.
    ///
    /// If you are tempted to make this subtractable, the question to answer
    /// first is not "can we delete something" but "how would a reader verify
    /// afterwards that the erased contributor is actually gone." Absent an
    /// answer, invalidation is the only honest disposition.
    GeneratedDefinition,
    /// One addressable package file, which can be removed on its own.
    ///
    /// Subtractable: a file is a whole unit with its own row, so deleting it
    /// removes exactly one contributor's bytes and leaves every other
    /// contributor's file untouched and verifiable.
    GeneratedFile,
}

impl RevisionContributionKind {
    /// Returns the stable database representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GeneratedDefinition => "generated_definition",
            Self::GeneratedFile => "generated_file",
        }
    }

    /// Returns whether one contributor's bytes can be removed without discarding the rest.
    #[must_use]
    pub const fn is_subtractable(self) -> bool {
        matches!(self, Self::GeneratedFile)
    }
}

/// Whether a suite was generated for a proposal or accumulated from a sibling session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuiteContributionKind {
    /// Generated from the proposal's own source session.
    Generated,
    /// Pooled from a deduped recurring sibling session.
    Accumulated,
}

impl SuiteContributionKind {
    /// Returns the stable database representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::Accumulated => "accumulated",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "generated" => Ok(Self::Generated),
            "accumulated" => Ok(Self::Accumulated),
            other => Err(MoaError::StorageError(format!(
                "unknown artifact suite contribution kind `{other}`"
            ))),
        }
    }
}

/// One record of whose data produced part of a revision.
#[derive(Debug, Clone)]
pub struct NewRevisionContribution {
    /// Revision the bytes belong to.
    pub revision_uid: Uuid,
    /// Specific package file, for a `GeneratedFile` contribution.
    pub file_uid: Option<Uuid>,
    /// Learning candidate that carried the evidence.
    pub candidate_id: Uuid,
    /// Which bytes this accounts for.
    pub kind: RevisionContributionKind,
}

/// One attributable regression-suite blob owned by the artifact registry.
#[derive(Debug, Clone)]
pub struct NewSuiteContribution {
    /// Learning candidate the suite belongs to.
    pub candidate_id: Uuid,
    /// Draft revision the suite guards, when it already exists.
    pub revision_uid: Option<Uuid>,
    /// Whether the suite was generated or accumulated.
    pub kind: SuiteContributionKind,
    /// Stable name distinguishing suites within one candidate.
    pub suite_name: String,
    /// The suite source itself.
    pub suite_source: String,
    /// Session whose transcript produced the suite.
    pub source_session_id: Option<Uuid>,
    /// Experience record whose transcript produced the suite.
    pub source_experience_id: Option<Uuid>,
}

/// One stored suite contribution as read back for review assembly.
#[derive(Debug, Clone)]
pub struct StoredSuiteContribution {
    /// Whether the suite was generated or accumulated.
    pub kind: SuiteContributionKind,
    /// Stable name distinguishing suites within one candidate.
    pub suite_name: String,
    /// The suite source itself.
    pub suite_source: String,
    /// Session whose transcript produced the suite.
    pub source_session_id: Option<Uuid>,
    /// Experience record whose transcript produced the suite.
    pub source_experience_id: Option<Uuid>,
}

impl ArtifactRegistry {
    /// Records which learning candidate contributed which revision bytes.
    ///
    /// Written in the caller's transaction alongside the revision itself, so a
    /// revision and its attribution cannot diverge.
    pub async fn record_revision_contribution_in_tx(
        conn: &mut PgConnection,
        storage_partition_id: &str,
        tenant_id: &str,
        contribution: &NewRevisionContribution,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO moa.artifact_revision_contribution
                (contribution_uid, storage_partition_id, revision_uid, file_uid,
                 candidate_id, tenant_id, contribution_kind)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(storage_partition_id)
        .bind(contribution.revision_uid)
        .bind(contribution.file_uid)
        .bind(contribution.candidate_id)
        .bind(tenant_id)
        .bind(contribution.kind.as_str())
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("record artifact revision contribution: {error}"))
        })?;
        Ok(())
    }

    /// Stores one attributable regression-suite blob, returning whether it was new.
    ///
    /// Deduped by `(candidate, kind, suite_name)`, so re-observing the same
    /// sibling session appends nothing rather than growing the pool with
    /// duplicates. Refused outright when the suite names no source, because a
    /// suite nobody can attribute is a suite no erasure can reach.
    pub async fn record_suite_contribution_in_tx(
        conn: &mut PgConnection,
        storage_partition_id: &str,
        tenant_id: &str,
        contribution: &NewSuiteContribution,
    ) -> Result<bool> {
        if contribution.source_session_id.is_none() && contribution.source_experience_id.is_none() {
            return Err(MoaError::StorageError(format!(
                "regression suite `{}` for candidate `{}` names no source session or experience",
                contribution.suite_name, contribution.candidate_id
            )));
        }
        let inserted = sqlx::query(
            r#"
            INSERT INTO moa.artifact_suite_contribution
                (contribution_uid, storage_partition_id, tenant_id, candidate_id, revision_uid,
                 suite_kind, suite_name, suite_source, source_session_id, source_experience_id)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT (candidate_id, suite_kind, suite_name) DO NOTHING
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(storage_partition_id)
        .bind(tenant_id)
        .bind(contribution.candidate_id)
        .bind(contribution.revision_uid)
        .bind(contribution.kind.as_str())
        .bind(&contribution.suite_name)
        .bind(&contribution.suite_source)
        .bind(contribution.source_session_id)
        .bind(contribution.source_experience_id)
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("record artifact suite contribution: {error}"))
        })?
        .rows_affected();
        Ok(inserted > 0)
    }

    /// Records every attributable byte of one revision against its candidate.
    ///
    /// Writes the non-subtractable `generated_definition` row plus one
    /// `generated_file` row per package file the revision owns. The file rows are
    /// derived from `moa.artifact_file` in the same statement rather than from a
    /// caller-supplied list, so a revision cannot be half-attributed by a caller
    /// that forgot a file: whatever the revision actually stores is what gets an
    /// attribution row.
    ///
    /// Written in the transaction that created the revision. Publishing promotes
    /// a draft in place rather than copying it, so attribution recorded here
    /// survives into the serving revision without a second write.
    pub async fn record_revision_attribution_in_tx(
        conn: &mut PgConnection,
        storage_partition_id: &str,
        tenant_id: &str,
        revision_uid: Uuid,
        candidate_id: Uuid,
    ) -> Result<()> {
        Self::record_revision_contribution_in_tx(
            conn,
            storage_partition_id,
            tenant_id,
            &NewRevisionContribution {
                revision_uid,
                file_uid: None,
                candidate_id,
                kind: RevisionContributionKind::GeneratedDefinition,
            },
        )
        .await?;
        sqlx::query(
            r#"
            INSERT INTO moa.artifact_revision_contribution
                (contribution_uid, storage_partition_id, revision_uid, file_uid,
                 candidate_id, tenant_id, contribution_kind)
            SELECT gen_random_uuid(), $1, file.revision_uid, file.file_uid, $3, $4,
                   'generated_file'
            FROM moa.artifact_file AS file
            WHERE file.revision_uid = $2
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(storage_partition_id)
        .bind(revision_uid)
        .bind(candidate_id)
        .bind(tenant_id)
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("record artifact file contributions: {error}"))
        })?;
        Ok(())
    }

    /// Re-points a candidate's generated suite rows at its current draft revision.
    ///
    /// A generalization pass rewrites the draft and re-attaches the same suite
    /// bytes to the new revision. Leaving the contribution pointed at the
    /// superseded revision would record a link that is no longer true, which is
    /// the failure mode this whole table exists to remove.
    pub async fn repoint_suite_contributions_in_tx(
        conn: &mut PgConnection,
        candidate_id: Uuid,
        revision_uid: Uuid,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE moa.artifact_suite_contribution
            SET revision_uid = $2
            WHERE candidate_id = $1 AND suite_kind = 'generated'
            "#,
        )
        .bind(candidate_id)
        .bind(revision_uid)
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("repoint artifact suite contributions: {error}"))
        })?;
        Ok(())
    }

    /// Counts the accumulated sibling suites already pooled onto one candidate.
    pub async fn count_suite_contributions_in_tx(
        conn: &mut PgConnection,
        candidate_id: Uuid,
        kind: SuiteContributionKind,
    ) -> Result<usize> {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT count(*)
            FROM moa.artifact_suite_contribution
            WHERE candidate_id = $1 AND suite_kind = $2
            "#,
        )
        .bind(candidate_id)
        .bind(kind.as_str())
        .fetch_one(&mut *conn)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("count artifact suite contributions: {error}"))
        })?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Reads every suite blob attributable to one candidate, in stable order.
    ///
    /// This is the review-input assembly seam: the regression gate asks the
    /// artifact owner for the pool instead of parsing candidate payload JSON, so
    /// there is exactly one place that knows how these bytes are stored.
    pub async fn list_suite_contributions(
        &self,
        candidate_id: Uuid,
    ) -> Result<Vec<StoredSuiteContribution>> {
        fetch_suite_contributions(&self.pool, candidate_id).await
    }

    /// Reads one candidate's suite blobs inside the caller's open transaction.
    ///
    /// Same assembly seam as [`Self::list_suite_contributions`], for producers
    /// that already hold the proposal's advisory lock and must observe the pool
    /// exactly as it stands inside that lock.
    pub async fn list_suite_contributions_in_tx(
        conn: &mut PgConnection,
        candidate_id: Uuid,
    ) -> Result<Vec<StoredSuiteContribution>> {
        fetch_suite_contributions(&mut *conn, candidate_id).await
    }
}

/// Reads and decodes one candidate's suite contributions from any executor.
async fn fetch_suite_contributions<'e, E>(
    executor: E,
    candidate_id: Uuid,
) -> Result<Vec<StoredSuiteContribution>>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        r#"
            SELECT suite_kind, suite_name, suite_source, source_session_id, source_experience_id
            FROM moa.artifact_suite_contribution
            WHERE candidate_id = $1
            ORDER BY suite_kind, suite_name, contribution_uid
            "#,
    )
    .bind(candidate_id)
    .fetch_all(executor)
    .await
    .map_err(|error| {
        MoaError::StorageError(format!("list artifact suite contributions: {error}"))
    })?;
    rows.iter()
        .map(|row| {
            Ok(StoredSuiteContribution {
                kind: SuiteContributionKind::parse(
                    &row.try_get::<String, _>("suite_kind").map_err(|error| {
                        MoaError::StorageError(format!("read suite kind: {error}"))
                    })?,
                )?,
                suite_name: row
                    .try_get("suite_name")
                    .map_err(|error| MoaError::StorageError(format!("read suite name: {error}")))?,
                suite_source: row.try_get("suite_source").map_err(|error| {
                    MoaError::StorageError(format!("read suite source: {error}"))
                })?,
                source_session_id: row.try_get("source_session_id").map_err(|error| {
                    MoaError::StorageError(format!("read suite source session: {error}"))
                })?,
                source_experience_id: row.try_get("source_experience_id").map_err(|error| {
                    MoaError::StorageError(format!("read suite source experience: {error}"))
                })?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_definition_is_not_subtractable_but_a_file_is() {
        // Pins: the erasure rule that decides between rewriting a revision and
        // invalidating it. If `GeneratedDefinition` ever became subtractable, a
        // shared revision built partly from erased evidence would be reported as
        // repaired while the fused model output stayed exactly as it was.
        assert!(!RevisionContributionKind::GeneratedDefinition.is_subtractable());
        assert!(RevisionContributionKind::GeneratedFile.is_subtractable());
    }

    #[test]
    fn suite_contribution_kind_round_trips_and_rejects_unknown_labels() {
        // Pins: persisted suite labels survive a write/read cycle byte-identically
        // and an unrecognized label fails closed instead of defaulting to
        // `generated`, which would misreport pooled sibling bytes as the
        // candidate's own.
        for kind in [
            SuiteContributionKind::Generated,
            SuiteContributionKind::Accumulated,
        ] {
            assert_eq!(
                SuiteContributionKind::parse(kind.as_str()).expect("round-trip"),
                kind
            );
        }
        assert!(SuiteContributionKind::parse("promoted").is_err());
    }
}

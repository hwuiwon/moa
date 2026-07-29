//! End-to-end tests for the skill review-time regression gate.
//!
//! These drive `LearningReview` acceptance through real eval-engine execution
//! against the scripted mock provider: the generated regression suite actually
//! runs, so the gate's candidate-only and compared-with-previous paths are
//! exercised for real instead of being stubbed at the report boundary. They
//! need the compose Postgres and the `provider-overrides` feature, but no
//! Restate server and no billed tokens.
//!
//! Run recipe:
//! `MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa \
//!  cargo nextest run -p moa-orchestrator --test skill_learning_gate_e2e \
//!  --features provider-overrides --run-ignored all`

#![recursion_limit = "256"]

use std::{collections::HashMap, sync::Arc};

use moa_artifacts::document::ArtifactStatus;
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::validation::validate_for_status;
use moa_config::MoaConfig;
use moa_core::{
    events::Event, types::action_policy::ActionRuleScope, types::channel::Attachment,
    types::contact::SessionActorRef, types::events_stream::EventRecord,
    types::experience::LearningCandidate, types::experience::LearningCandidateSourceRef,
    types::experience::LearningCandidateStatus, types::experience::LearningCandidateType,
    types::experience::LearningProposalKind, types::experience::LearningRiskClass,
    types::identifiers::ModelId, types::identifiers::SegmentId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::provider::ModelTier,
};
use moa_hands::{ToolRegistry, ToolRouter};
use moa_memory_pii::HeuristicPiiClassifier;
use moa_orchestrator::services::learning_review::accept_skill_candidate_after_authz;
use moa_skills::artifact::skill_artifact_document_from_package;
use moa_skills::evidence::{EvidenceScope, SegmentNarrative, sanitize_segment_evidence};
use moa_skills::package::{SkillPackage, SkillPackageFile, ValidatedSkillPackage};
use moa_skills::regression::generate_skill_test_suite_source_for_name;
use moa_test_support::fixtures::tenant_id_from_storage_partition_id;
use moa_test_support::postgres::bootstrap_test_db;
use moa_wire::session_store::{LearningCandidateReviewAction, LearningCandidateReviewRequest};
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

mod skill_learning_gate {
    use super::*;

    #[tokio::test]
    #[ignore = "requires compose Postgres and the provider-overrides feature"]
    async fn accept_runs_candidate_only_gate_and_promotes_new_skill_e2e() {
        // Pins: a Created candidate with no previous active skill executes its generated
        // suite alone through the real eval engine and promotes with a candidate-only
        // execution record and derived acceptance checks. The suite deliberately omits
        // `default_timeout_seconds`: the gate must floor the zero TOML default instead
        // of instantly timing out every case and rejecting for a fixture defect.
        let harness = GateHarness::bootstrap("gate-candidate-only").await;
        let package = skill_package(&harness.skill_name, "Candidate-only gate skill");
        let draft = harness.create_draft(&package).await;
        let skill_name = harness.skill_name.clone();
        let timeoutless_suite = format!(
            "[suite]\nname = \"{skill_name}-regression\"\n\n\
             [[cases]]\nname = \"smoke\"\ninput = \"run the recorded workflow\"\n"
        );
        let candidate = harness
            .append_proposed_candidate("skill_created", &draft, Some(&timeoutless_suite))
            .await;

        let response = harness.accept(candidate.id).await.expect("accept promotes");

        let evaluation = harness.reload_evaluation(candidate.id).await;
        assert_eq!(response.status, LearningCandidateStatus::Promoted);
        assert_eq!(evaluation["regression_execution"], "completed");
        assert_eq!(
            evaluation["regression_report"]["execution_mode"],
            "candidate_only"
        );
        assert_eq!(evaluation["regression_report"]["decision"], "accepted");
        assert_eq!(
            evaluation["regression_report"]["candidate"]["failed_runs"], 0,
            "the candidate suite must actually execute and pass"
        );
        assert_eq!(
            evaluation["acceptance_checks"]["held_out_pass"],
            serde_json::Value::Bool(true)
        );
        assert!(
            evaluation["acceptance_checks"]["held_out_description"]
                .as_str()
                .expect("held-out description is recorded")
                .contains("smoke gate"),
            "acceptance checks must describe the candidate-only run honestly"
        );
        let published = harness.load_revision(draft.revision_uid).await;
        assert_eq!(published.status, ArtifactStatus::Published);
    }

    /// Identifiers planted in the source transcript this gate test learns from.
    const GATE_USER_EMAIL: &str = "alice@example.com";
    const GATE_QUEUED_EMAIL: &str = "carol@example.com";
    const GATE_SUMMARY_EMAIL: &str = "dave@example.com";

    #[tokio::test]
    #[ignore = "requires compose Postgres and the provider-overrides feature"]
    async fn accept_promotes_a_suite_generated_from_sanitized_evidence_e2e() {
        // Pins: a proposal whose regression suite was produced by the real sanitized
        // generation path survives review end to end — the gate executes it, the
        // candidate promotes, and the suite that was stored, executed, and reviewed
        // carries redaction placeholders instead of the identifiers that were in the
        // source transcript. This is the whole contract in one flow: sanitizing the
        // learning corpus must not cost the loop its ability to gate.
        let harness = GateHarness::bootstrap("gate-sanitized").await;
        let package = skill_package(&harness.skill_name, "Sanitized-evidence gate skill");
        let draft = harness.create_draft(&package).await;

        let summary = format!("reset the account for {GATE_SUMMARY_EMAIL}");
        let evidence = sanitize_segment_evidence(
            &HeuristicPiiClassifier,
            EvidenceScope {
                tenant_id: tenant_id_from_storage_partition_id(&harness.storage_partition_id),
                contact_id: None,
                session_id: SessionId::new(),
                segment_id: SegmentId::new(),
                experience_id: Uuid::now_v7(),
            },
            &gate_source_transcript(),
            SegmentNarrative {
                task_summary: Some(&summary),
                assessment_summaries: &[],
            },
        )
        .await
        .expect("the source transcript's only sensitivity is redactable PII");

        let generated = generate_skill_test_suite_source_for_name(
            tenant_id_from_storage_partition_id(&harness.storage_partition_id),
            &harness.skill_name,
            &evidence,
        )
        .expect("generate the suite through the real path");
        for planted in [GATE_USER_EMAIL, GATE_QUEUED_EMAIL, GATE_SUMMARY_EMAIL] {
            assert!(
                !generated.source_toml.contains(planted),
                "{planted} reached the generated suite: {}",
                generated.source_toml
            );
        }

        let candidate = harness
            .append_proposed_candidate("skill_created", &draft, Some(&generated.source_toml))
            .await;

        let response = harness
            .accept(candidate.id)
            .await
            .expect("a sanitized suite is still a gateable suite");

        assert_eq!(response.status, LearningCandidateStatus::Promoted);
        let evaluation = harness.reload_evaluation(candidate.id).await;
        assert_eq!(evaluation["regression_execution"], "completed");
        assert_eq!(evaluation["regression_report"]["decision"], "accepted");
        assert_eq!(
            evaluation["regression_report"]["candidate"]["failed_runs"], 0,
            "the generated suite must actually execute and pass"
        );
        let published = harness.load_revision(draft.revision_uid).await;
        assert_eq!(published.status, ArtifactStatus::Published);

        // The reviewed row is the durable artifact of this decision, so the
        // redaction has to hold there and not only in the in-memory suite.
        let reloaded = harness.reload_candidate(candidate.id).await;
        let stored = serde_json::to_string(&reloaded.payload).expect("serialize candidate payload")
            + &serde_json::to_string(&reloaded.evaluation_payload)
                .expect("serialize evaluation payload");
        for planted in [GATE_USER_EMAIL, GATE_QUEUED_EMAIL, GATE_SUMMARY_EMAIL] {
            assert!(
                !stored.contains(planted),
                "{planted} survived into the reviewed rows: {stored}"
            );
        }
        assert!(
            stored.contains("[EMAIL_REDACTED]"),
            "the reviewed suite should carry the redaction placeholder: {stored}"
        );
    }

    /// A source transcript carrying an identifier in each caller-authored carrier.
    ///
    /// The assistant response is deliberately a single word that the scripted mock
    /// provider's own output contains, so the generated keyword oracle is
    /// satisfiable and the gate exercises a real pass rather than a fixture-shaped
    /// failure. There are no tool calls, so the generated trajectory is empty and
    /// matches the mock run.
    fn gate_source_transcript() -> Vec<EventRecord> {
        let session_id = SessionId::new();
        let make = |sequence_num: u64, event: Event| EventRecord {
            id: Uuid::now_v7(),
            session_id,
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: moa_test_support::fixtures::pg_now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        };
        vec![
            make(
                1,
                Event::UserMessage {
                    text: format!("reset the account for {GATE_USER_EMAIL} and confirm"),
                    attachments: Vec::<Attachment>::new(),
                },
            ),
            make(
                2,
                Event::QueuedMessage {
                    text: format!("also update {GATE_QUEUED_EMAIL}"),
                    attachments: Vec::<Attachment>::new(),
                    queued_at: moa_test_support::fixtures::pg_now(),
                },
            ),
            make(
                3,
                Event::BrainResponse {
                    text: "response".to_string(),
                    thought_signature: None,
                    model: ModelId::new("scripted-mock"),
                    model_tier: ModelTier::Auxiliary,
                    input_tokens_uncached: 8,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 8,
                    cost_cents: 0,
                    duration_ms: 1,
                    llm_ttft_ms: None,
                },
            ),
        ]
    }

    #[tokio::test]
    #[ignore = "requires compose Postgres and the provider-overrides feature"]
    async fn accept_compares_against_published_previous_revision_e2e() {
        // Pins: when a published previous revision exists, the gate executes both the
        // previous and candidate suites and promotes on a no-regression comparison —
        // and the previous revision's own suite (riding its package) executes as
        // held-out material the candidate was not derived from.
        let harness = GateHarness::bootstrap("gate-compared").await;
        let previous = skill_package_with_suite(&harness.skill_name, "Previous published revision");
        harness.create_and_publish(&previous).await;
        let improved = skill_package(&harness.skill_name, "Improved candidate revision");
        let draft = harness.create_draft(&improved).await;
        let candidate = harness
            .append_proposed_candidate("skill_improved", &draft, None)
            .await;

        let response = harness.accept(candidate.id).await.expect("accept promotes");

        assert_eq!(response.status, LearningCandidateStatus::Promoted);
        let evaluation = harness.reload_evaluation(candidate.id).await;
        assert_eq!(
            evaluation["regression_report"]["execution_mode"],
            "compared_with_previous"
        );
        assert_eq!(evaluation["regression_report"]["decision"], "accepted");
        assert!(
            evaluation["regression_report"]["previous"]["total_runs"]
                .as_u64()
                .is_some_and(|runs| runs > 0),
            "the previous revision's suite must actually execute"
        );
        assert!(
            evaluation["regression_report"]["candidate"]["total_runs"]
                .as_u64()
                .is_some_and(|runs| runs > 0),
            "the candidate revision's suite must actually execute"
        );
        assert_eq!(
            evaluation["regression_report"]["held_out"]["source_count"], 1,
            "the previous revision's own suite must pool as held-out material"
        );
        assert_eq!(
            evaluation["regression_report"]["held_out"]["decision"],
            "accepted"
        );
        assert!(
            evaluation["regression_report"]["held_out"]["candidate"]["total_runs"]
                .as_u64()
                .is_some_and(|runs| runs > 0),
            "held-out pool cases must actually execute"
        );
        assert!(
            evaluation["acceptance_checks"]["held_out_description"]
                .as_str()
                .expect("held-out description is recorded")
                .contains("1 held-out suite source(s)"),
            "acceptance checks must credit the held-out pool honestly"
        );
    }

    #[tokio::test]
    #[ignore = "requires compose Postgres and the provider-overrides feature"]
    async fn accepted_execution_template_skill_compiles_audits_and_publishes_e2e() {
        // Pins: a template-bearing skill candidate compiles through moa-execution, persists
        // its strict preterminal planning audit, and publishes the same validated template.
        let harness = GateHarness::bootstrap("gate-execution-template").await;
        let package = execution_template_skill_package(&harness.skill_name);
        let draft = harness.create_draft(&package).await;
        let skill_name = &harness.skill_name;
        let suite = format!(
            "[suite]\nname = \"{skill_name}-regression\"\ndefault_timeout_seconds = 90\n\n\
             [[cases]]\nname = \"smoke\"\ninput = \"run the recorded workflow\"\n\
             [cases.metadata]\nexecution_input = {{}}\n"
        );
        let candidate = harness
            .append_proposed_candidate("skill_created", &draft, Some(&suite))
            .await;

        let response = harness.accept(candidate.id).await.expect("accept promotes");

        assert_eq!(response.status, LearningCandidateStatus::Promoted);
        let published = harness.load_revision(draft.revision_uid).await;
        assert_eq!(published.status, ArtifactStatus::Published);
        let moa_artifacts::document::ArtifactDefinition::Skill(definition) =
            published.document.definition
        else {
            panic!("published artifact must be a skill definition");
        };
        assert!(
            definition.execution_plan.is_some(),
            "published skill keeps its compiled execution-plan template"
        );
        let audits: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT source, outcome, operation_key \
             FROM moa.execution_compile_audit \
             WHERE tenant_id = $1 AND source = 'skill_regression'",
        )
        .bind(tenant_id_from_storage_partition_id(&harness.storage_partition_id).0)
        .fetch_all(harness.test_db.store().pool())
        .await
        .expect("load normalized skill-regression compile audit");
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].0, "skill_regression");
        assert_eq!(audits[0].1, "accepted");
        assert!(
            audits[0]
                .2
                .starts_with(&format!("skill_regression:{}:", draft.revision_uid))
        );
    }

    /// Shared fixtures for one gate acceptance flow against an isolated database.
    struct GateHarness {
        test_db: moa_test_support::postgres::TestDb,
        storage_partition_id: StoragePartitionId,
        skill_name: String,
        config: Arc<MoaConfig>,
        _memory_dir: TempDir,
    }

    impl GateHarness {
        async fn bootstrap(prefix: &str) -> Self {
            // Without the provider-overrides feature, ProviderRegistry::mock fails
            // loudly in accept(), so an explicit feature assertion is unnecessary.
            let test_db = bootstrap_test_db().await.expect("bootstrap gate test db");
            let storage_partition_id = StoragePartitionId::new(Uuid::now_v7().to_string());
            let skill_name = format!("{prefix}-{}", Uuid::now_v7().simple());
            let memory_dir = tempfile::tempdir().expect("create gate memory dir");
            let mut config = MoaConfig::default();
            config.database.url = test_db.database_url().to_string();
            config.local.memory_dir = memory_dir
                .path()
                .join("memory")
                .to_string_lossy()
                .into_owned();
            config.query_rewrite.enabled = false;
            Self {
                test_db,
                storage_partition_id,
                skill_name,
                config: Arc::new(config),
                _memory_dir: memory_dir,
            }
        }

        fn scope(&self) -> ActionRuleScope {
            ActionRuleScope::Tenant {
                tenant_id: tenant_id_from_storage_partition_id(&self.storage_partition_id),
            }
        }

        async fn accept(
            &self,
            candidate_id: Uuid,
        ) -> Result<
            moa_wire::session_store::LearningCandidateReviewResponse,
            Box<dyn std::error::Error>,
        > {
            let store = Arc::new(self.test_db.store().clone());
            let providers = Arc::new(
                moa_providers::ProviderRegistry::mock(7).expect("build scripted mock registry"),
            );
            accept_skill_candidate_after_authz(
                store.clone(),
                store.pool().clone(),
                self.config.clone(),
                providers,
                Arc::new(ToolRouter::new(
                    ToolRegistry::default_local(),
                    HashMap::new(),
                    moa_hands::local_development_sandbox_policy(),
                )),
                LearningCandidateReviewRequest {
                    tenant_id: tenant_id_from_storage_partition_id(&self.storage_partition_id),
                    candidate_id,
                    action: LearningCandidateReviewAction::Accept,
                    reviewer_subject: "user:gate-reviewer".to_string(),
                    reason: Some("gate e2e".to_string()),
                },
            )
            .await
            .map_err(|error| format!("{error:?}").into())
        }

        async fn create_draft(
            &self,
            package: &ValidatedSkillPackage,
        ) -> moa_artifacts::registry::StoredArtifactRevision {
            let document = skill_artifact_document_from_package(package, ArtifactStatus::Draft)
                .expect("build skill artifact document");
            let source = document.to_yaml().expect("render artifact yaml");
            let files = package
                .files
                .iter()
                .map(|file| NewArtifactFile {
                    path: file.path.clone(),
                    content: file.content.clone(),
                    content_type: file.content_type.clone(),
                    executable: file.executable,
                })
                .collect::<Vec<_>>();
            ArtifactRegistry::new(self.test_db.store().pool().clone())
                .create_draft(
                    &self.scope(),
                    NewArtifactDraft {
                        document: &document,
                        source_format: "yaml",
                        source_text: source.as_bytes(),
                        files: &files,
                    },
                )
                .await
                .expect("create draft skill artifact")
        }

        async fn create_and_publish(&self, package: &ValidatedSkillPackage) {
            let draft = self.create_draft(package).await;
            let report = validate_for_status(&draft.document, ArtifactStatus::Published);
            assert!(report.is_ok(), "previous revision must be publishable");
            ArtifactRegistry::new(self.test_db.store().pool().clone())
                .publish_revision(&self.scope(), draft.revision_uid, &report)
                .await
                .expect("publish previous skill revision");
        }

        async fn append_proposed_candidate(
            &self,
            operation: &str,
            draft: &moa_artifacts::registry::StoredArtifactRevision,
            suite_override: Option<&str>,
        ) -> LearningCandidate {
            let now = moa_test_support::fixtures::pg_now();
            let skill_name = &self.skill_name;
            let suite = suite_override.map(ToString::to_string).unwrap_or_else(|| {
                format!(
                    "[suite]\nname = \"{skill_name}-regression\"\ndefault_timeout_seconds = 90\n\n\
                     [[cases]]\nname = \"smoke\"\ninput = \"run the recorded workflow\"\n"
                )
            });
            let mut payload = json!({
                "kind": "skill_draft_proposal",
                "operation": operation,
                "artifact_uid": draft.artifact_uid,
                "draft_artifact_revision_uid": draft.revision_uid,
                "artifact_kind": "skill",
                "artifact_name": skill_name,
                "artifact_path": format!("skills/{skill_name}/SKILL.md"),
                "source_session_id": Uuid::now_v7(),
            });
            if operation == "skill_improved" {
                payload["previous_version"] = json!("1.0");
            }
            let candidate = LearningCandidate {
                id: Uuid::now_v7(),
                tenant_id: tenant_id_from_storage_partition_id(&self.storage_partition_id),
                user_id: None,
                candidate_type: LearningCandidateType::Skill,
                proposal_kind: LearningProposalKind::SkillDraft,
                status: LearningCandidateStatus::Proposed,
                target_id: Some(format!("skills/{skill_name}/SKILL.md")),
                target_label: Some(skill_name.to_string()),
                task_fingerprint: None,
                task_facets: None,
                payload,
                evaluation_payload: None,
                // The draft revision this proposal was generated into is a real
                // row in this fixture, so it is the honest typed source.
                sources: vec![LearningCandidateSourceRef::ArtifactRevision {
                    revision_uid: draft.revision_uid,
                }],
                confidence: Some(0.9),
                risk_class: LearningRiskClass::Low,
                promotion_requirements: vec!["human_review".to_string()],
                status_reason: None,
                batch_id: None,
                created_at: now,
                updated_at: now,
            };
            self.test_db
                .store()
                .append_learning_candidate(&candidate)
                .await
                .expect("append gate candidate");
            // The gate reads its suite from the artifact owner's contribution
            // rows, so the fixture writes the row the production filing path
            // writes. The source session is real because the contribution's
            // source columns carry foreign keys: a suite nobody can attribute is
            // a suite no erasure can reach, and the table refuses to store one.
            let source_session_id = self.create_session().await;
            let mut conn = self
                .test_db
                .store()
                .pool()
                .acquire()
                .await
                .expect("acquire connection for suite contribution");
            ArtifactRegistry::record_suite_contribution_in_tx(
                &mut conn,
                self.storage_partition_id.as_str(),
                &candidate.tenant_id.to_string(),
                &moa_artifacts::registry::NewSuiteContribution {
                    candidate_id: candidate.id,
                    revision_uid: Some(draft.revision_uid),
                    kind: moa_artifacts::registry::SuiteContributionKind::Generated,
                    suite_name: format!("skills/{skill_name}/tests/suite.toml"),
                    suite_source: suite,
                    source_session_id: Some(source_session_id.0),
                    source_experience_id: None,
                },
            )
            .await
            .expect("record the candidate's generated suite contribution");
            candidate
        }

        /// Creates the real session the suite contribution's source column names.
        ///
        /// `created_by` is not boilerplate: session creation refuses a session
        /// with neither a contact nor a creator, because an unattributed session
        /// is one no erasure could enter through. That is the same guarantee the
        /// contribution's foreign key enforces one level up — which is why this
        /// fixture cannot fabricate a session id any more than it can fabricate
        /// an experience id.
        async fn create_session(&self) -> SessionId {
            let meta = moa_core::types::session::SessionMeta {
                tenant_id: tenant_id_from_storage_partition_id(&self.storage_partition_id),
                created_by: Some(SessionActorRef::Identity {
                    id: Uuid::from_u128(1),
                }),
                model: ModelId::new("test-model"),
                ..moa_core::types::session::SessionMeta::default()
            };
            moa_core::traits::SessionStore::create_session(self.test_db.store(), meta)
                .await
                .expect("create source session for the suite contribution")
        }

        async fn reload_candidate(&self, candidate_id: Uuid) -> LearningCandidate {
            self.test_db
                .store()
                .get_learning_candidate(
                    &tenant_id_from_storage_partition_id(&self.storage_partition_id),
                    candidate_id,
                )
                .await
                .expect("reload candidate")
                .expect("candidate exists")
        }

        async fn reload_evaluation(&self, candidate_id: Uuid) -> serde_json::Value {
            self.test_db
                .store()
                .get_learning_candidate(
                    &tenant_id_from_storage_partition_id(&self.storage_partition_id),
                    candidate_id,
                )
                .await
                .expect("reload candidate")
                .expect("candidate exists")
                .evaluation_payload
                .expect("promotion evaluation payload")
        }

        async fn load_revision(
            &self,
            revision_uid: Uuid,
        ) -> moa_artifacts::registry::StoredArtifactRevision {
            ArtifactRegistry::new(self.test_db.store().pool().clone())
                .load_revision(&self.scope(), revision_uid)
                .await
                .expect("load revision")
                .expect("revision exists")
        }
    }

    fn skill_package(skill_name: &str, description: &str) -> ValidatedSkillPackage {
        let markdown = format!(
            "---\n\
             name: {skill_name}\n\
             description: \"{description}\"\n\
             allowed-tools: bash file_read\n\
             metadata:\n\
             \x20 moa-version: \"1.0\"\n\
             \x20 moa-tags: \"gate, e2e\"\n\
             \x20 moa-estimated-tokens: \"300\"\n\
             ---\n\n\
             # {skill_name}\n\n\
             Follow the recorded workflow steps and verify the result.\n"
        );
        SkillPackage::from_skill_markdown(markdown)
            .validate()
            .expect("gate skill package validates")
    }

    /// A skill package that carries its own regression suite, as every promoted
    /// revision does once proposals ride their generated suites.
    fn skill_package_with_suite(skill_name: &str, description: &str) -> ValidatedSkillPackage {
        let base = skill_package(skill_name, description);
        let suite = format!(
            "[suite]\nname = \"{skill_name}-regression\"\ndefault_timeout_seconds = 90\n\n\
             [[cases]]\nname = \"held-out-smoke\"\ninput = \"run the earlier recorded workflow\"\n"
        );
        let mut files = base
            .files
            .iter()
            .map(|file| SkillPackageFile {
                path: file.path.clone(),
                content: file.content.clone(),
                content_type: file.content_type.clone(),
                executable: file.executable,
            })
            .collect::<Vec<_>>();
        files.push(
            SkillPackageFile::new("tests/regression-suite.toml", suite.into_bytes())
                .with_content_type("application/toml; charset=utf-8"),
        );
        SkillPackage::new(files)
            .validate()
            .expect("suite-carrying gate package validates")
    }

    /// A skill package whose `skill.moa.yaml` carries a minimal output-only execution plan.
    fn execution_template_skill_package(skill_name: &str) -> ValidatedSkillPackage {
        let markdown = format!(
            "---\n\
             name: {skill_name}\n\
             description: \"Execution-template gate skill\"\n\
             allowed-tools: bash\n\
             metadata:\n\
             \x20 moa-version: \"1.0\"\n\
             \x20 moa-tags: \"gate, execution-template\"\n\
             \x20 moa-estimated-tokens: \"300\"\n\
             ---\n\n\
             # {skill_name}\n\n\
             Run the deterministic execution template.\n"
        );
        let skill_yaml = "\
inputs:
  type: object
  additionalProperties: false
outputs:
  type: object
allowed_tools:
  - bash
execution_plan:
  goal:
    requirements:
      - id: regression_result
        description: Return the deterministic regression result.
    deliverables: []
    coverage: []
    constraints: []
    completion_checks:
      - id: output_schema
        description: Validate the regression output.
        requirement_ids: [regression_result]
        constraint_ids: []
        kind:
          kind: output_schema
  plan:
    schema_version: 1
    input_schema:
      type: object
      additionalProperties: false
    output_schema:
      type: object
    nodes:
      - id: result
        requirement_ids: [regression_result]
        depends_on: []
        input: {}
        output_schema:
          type: object
        operation:
          kind: output
          value:
            route: done
        retry:
          max_attempts: 1
          initial_backoff_ms: 0
          max_backoff_ms: 0
";
        SkillPackage::new(vec![
            SkillPackageFile::new("SKILL.md", markdown.into_bytes())
                .with_content_type("text/markdown; charset=utf-8"),
            SkillPackageFile::new(
                moa_skills::artifact::SKILL_ARTIFACT_PATH,
                skill_yaml.as_bytes().to_vec(),
            )
            .with_content_type("application/yaml; charset=utf-8"),
        ])
        .validate()
        .expect("gate execution-template package validates")
    }
}

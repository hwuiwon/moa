//! Heuristic memory consolidation and scheduled maintenance helpers.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use moa_core::{
    ConfidenceLevel, MemoryPath, MemoryScope, MemoryStore, Result, SessionFilter, SessionStatus,
    SessionStore, WikiPage,
};
use regex::{Captures, Regex};

use crate::FileMemoryStore;
use crate::index::{LogChange, LogEntry, last_operation_timestamp};

const CONSOLIDATION_OPERATION: &str = "consolidation";

/// Outcome of a single consolidation pass over a memory scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidationReport {
    /// Scope that was consolidated.
    pub scope: MemoryScope,
    /// Number of pages rewritten in place.
    pub pages_updated: usize,
    /// Number of pages removed as stale.
    pub pages_deleted: usize,
    /// Number of relative date phrases normalized.
    pub relative_dates_normalized: usize,
    /// Number of contradictory claims rewritten.
    pub contradictions_resolved: usize,
    /// Number of pages whose confidence was decayed.
    pub confidence_decayed: usize,
    /// Paths with no inbound references after consolidation.
    pub orphaned_pages: Vec<MemoryPath>,
    /// `MEMORY.md` line count before regeneration.
    pub memory_lines_before: usize,
    /// `MEMORY.md` line count after regeneration.
    pub memory_lines_after: usize,
}

impl ConsolidationReport {
    /// Creates a new empty report for a scope.
    pub fn empty(scope: MemoryScope) -> Self {
        Self {
            scope,
            pages_updated: 0,
            pages_deleted: 0,
            relative_dates_normalized: 0,
            contradictions_resolved: 0,
            confidence_decayed: 0,
            orphaned_pages: Vec::new(),
            memory_lines_before: 0,
            memory_lines_after: 0,
        }
    }
}

/// Runs every consolidation task directly against a scope.
pub async fn run_consolidation(
    store: &FileMemoryStore,
    scope: &MemoryScope,
) -> Result<ConsolidationReport> {
    let now = Utc::now();
    let mut report = ConsolidationReport::empty(scope.clone());
    let existing_index = store.get_index(scope).await?;
    report.memory_lines_before = existing_index.lines().count();

    let page_summaries = store.list_pages(scope, None).await?;
    let page_paths = page_summaries
        .iter()
        .map(|page| page.path.clone())
        .filter(|path| !is_internal_path(path))
        .collect::<Vec<_>>();

    let mut pages = Vec::with_capacity(page_paths.len());
    for path in &page_paths {
        pages.push((path.clone(), store.read_page(scope, path).await?));
    }

    let canonical_ports = canonical_port_claims(&pages);
    let inbound = inbound_reference_counts(&pages);
    let mut log_changes = Vec::new();

    for (path, mut page) in pages {
        let mut modified = false;
        if should_prune_page(&page) {
            store.delete_page(scope, &path).await?;
            report.pages_deleted += 1;
            log_changes.push(LogChange {
                action: "Pruned".to_string(),
                path,
                detail: Some("entity marked as non-existent".to_string()),
            });
            continue;
        }

        let (normalized_content, normalized_count) = normalize_relative_dates(&page.content, now);
        if normalized_count > 0 {
            page.content = normalized_content;
            page.updated = now;
            report.relative_dates_normalized += normalized_count;
            modified = true;
        }

        let (resolved_content, contradiction_count) =
            resolve_port_contradictions(&page.content, &canonical_ports);
        if contradiction_count > 0 {
            page.content = resolved_content;
            page.updated = now;
            report.contradictions_resolved += contradiction_count;
            modified = true;
        }

        if should_decay_confidence(&page, *inbound.get(path.as_str()).unwrap_or(&0), now) {
            page.confidence = decay_confidence(&page.confidence);
            page.updated = now;
            report.confidence_decayed += 1;
            modified = true;
        }

        if *inbound.get(path.as_str()).unwrap_or(&0) == 0 {
            report.orphaned_pages.push(path.clone());
        }

        if modified {
            store.write_page(scope, &path, page).await?;
            report.pages_updated += 1;
            log_changes.push(LogChange {
                action: "Updated".to_string(),
                path,
                detail: None,
            });
        }
    }

    store.refresh_scope_index(scope).await?;
    let refreshed_index = store.get_index(scope).await?;
    report.memory_lines_after = refreshed_index.lines().count();
    store.rebuild_search_index(scope).await?;
    store
        .append_scope_log(
            scope,
            LogEntry {
                timestamp: now,
                operation: CONSOLIDATION_OPERATION.to_string(),
                description: "Dream cycle".to_string(),
                changes: log_changes,
                brain_session: None,
            },
        )
        .await?;

    Ok(report)
}

/// Runs consolidation only for workspace scopes whose trigger conditions are met.
pub async fn run_due_consolidations<S: SessionStore + ?Sized>(
    store: &FileMemoryStore,
    session_store: &S,
) -> Result<Vec<ConsolidationReport>> {
    let sessions = session_store
        .list_sessions(SessionFilter::default())
        .await?;
    let mut workspace_ids = sessions
        .iter()
        .map(|session| session.workspace_id.clone())
        .collect::<Vec<_>>();
    workspace_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    workspace_ids.dedup();

    let mut reports = Vec::new();
    for workspace_id in workspace_ids {
        let scope = MemoryScope::Workspace {
            workspace_id: workspace_id.clone(),
        };
        if !tokio::fs::try_exists(store.scope_root(&scope)).await? {
            continue;
        }
        if !consolidation_due_for_scope(store, &scope, &sessions).await? {
            continue;
        }
        reports.push(run_consolidation(store, &scope).await?);
    }

    Ok(reports)
}

async fn consolidation_due_for_scope(
    store: &FileMemoryStore,
    scope: &MemoryScope,
    sessions: &[moa_core::SessionSummary],
) -> Result<bool> {
    let log = store.load_scope_log(scope).await?;
    let last_run = last_operation_timestamp(&log, CONSOLIDATION_OPERATION);
    if last_run.is_some_and(|timestamp| Utc::now() - timestamp < Duration::hours(24)) {
        return Ok(false);
    }

    let completed_since = sessions
        .iter()
        .filter(|session| {
            matches!(
                session.status,
                SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
            )
        })
        .filter(|session| match scope {
            MemoryScope::Global => false,
            MemoryScope::Workspace { workspace_id } => &session.workspace_id == workspace_id,
            MemoryScope::User {
                workspace_id,
                user_id,
            } => &session.workspace_id == workspace_id && &session.user_id == user_id,
        })
        .filter(|session| last_run.is_none_or(|timestamp| session.updated_at > timestamp))
        .count();

    Ok(completed_since >= 3)
}

fn normalize_relative_dates(content: &str, reference: DateTime<Utc>) -> (String, usize) {
    let replacements = [
        ("today", reference.date_naive()),
        ("yesterday", (reference - Duration::days(1)).date_naive()),
        ("tomorrow", (reference + Duration::days(1)).date_naive()),
        ("last week", (reference - Duration::days(7)).date_naive()),
        ("next week", (reference + Duration::days(7)).date_naive()),
        ("last month", (reference - Duration::days(30)).date_naive()),
        ("next month", (reference + Duration::days(30)).date_naive()),
    ];

    let mut normalized = content.to_string();
    let mut count = 0;

    for (phrase, date) in replacements {
        let Ok(regex) = Regex::new(&format!(r"(?i)\b{}\b", regex::escape(phrase))) else {
            continue;
        };
        let matches = regex.find_iter(&normalized).count();
        if matches > 0 {
            normalized = regex
                .replace_all(&normalized, date.format("%Y-%m-%d").to_string())
                .into_owned();
            count += matches;
        }
    }

    (normalized, count)
}

fn canonical_port_claims(pages: &[(MemoryPath, WikiPage)]) -> HashMap<String, String> {
    let mut claims = HashMap::<String, (&WikiPage, String)>::new();

    for (_, page) in pages {
        for (subject, port) in extract_port_claims(&page.content) {
            claims
                .entry(subject.clone())
                .and_modify(|(current_page, current_port)| {
                    if page.updated > current_page.updated
                        || (page.updated == current_page.updated
                            && confidence_rank(&page.confidence)
                                > confidence_rank(&current_page.confidence))
                    {
                        *current_page = page;
                        *current_port = port.clone();
                    }
                })
                .or_insert((page, port));
        }
    }

    claims
        .into_iter()
        .map(|(subject, (_, port))| (subject, port))
        .collect()
}

fn resolve_port_contradictions(
    content: &str,
    canonical_ports: &HashMap<String, String>,
) -> (String, usize) {
    let Some(regex) = port_claim_regex() else {
        return (content.to_string(), 0);
    };
    let mut rewritten = 0usize;

    let replaced = regex.replace_all(content, |captures: &Captures<'_>| {
        let subject =
            normalize_subject(captures.name("subject").map_or("", |value| value.as_str()));
        let Some(canonical) = canonical_ports.get(&subject) else {
            return captures[0].to_string();
        };
        let current = captures.name("port").map_or("", |value| value.as_str());
        if canonical == current {
            return captures[0].to_string();
        }
        rewritten += 1;
        format!(
            "{} runs on port {}",
            captures
                .name("subject")
                .map_or("", |value| value.as_str().trim()),
            canonical
        )
    });

    (replaced.into_owned(), rewritten)
}

fn extract_port_claims(content: &str) -> Vec<(String, String)> {
    let Some(regex) = port_claim_regex() else {
        return Vec::new();
    };

    regex
        .captures_iter(content)
        .filter_map(|captures| {
            let subject = captures.name("subject")?.as_str();
            let port = captures.name("port")?.as_str();
            Some((normalize_subject(subject), port.to_string()))
        })
        .collect()
}

fn port_claim_regex() -> Option<Regex> {
    Regex::new(
        r"(?i)\b(?P<subject>[A-Za-z][A-Za-z0-9 /_-]{0,80}?)\s+(?:runs|run|listens|listen|uses|use|serves|serve)\s+(?:on\s+)?port\s+(?P<port>\d{2,5})\b",
    )
    .ok()
}

fn inbound_reference_counts(pages: &[(MemoryPath, WikiPage)]) -> HashMap<String, usize> {
    let known_paths = pages
        .iter()
        .map(|(path, _)| path.as_str().to_string())
        .collect::<HashSet<_>>();
    let Ok(link_regex) = Regex::new(r"\[\[([^\]]+)\]\]") else {
        return HashMap::new();
    };
    let mut inbound = HashMap::<String, usize>::new();

    for (_, page) in pages {
        for target in page.related.iter().chain(page.sources.iter()) {
            if known_paths.contains(target) {
                *inbound.entry(target.clone()).or_default() += 1;
            }
        }

        for captures in link_regex.captures_iter(&page.content) {
            let Some(target) = captures.get(1) else {
                continue;
            };
            let normalized = target.as_str().trim();
            if known_paths.contains(normalized) {
                *inbound.entry(normalized.to_string()).or_default() += 1;
            }
        }
    }

    inbound
}

fn should_prune_page(page: &WikiPage) -> bool {
    matches!(page.page_type, moa_core::PageType::Entity)
        && page
            .metadata
            .get("entity_exists")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
}

fn should_decay_confidence(page: &WikiPage, inbound_refs: usize, now: DateTime<Utc>) -> bool {
    inbound_refs == 0
        && page.reference_count == 0
        && now - page.updated > Duration::days(30)
        && !matches!(page.confidence, ConfidenceLevel::Low)
}

fn decay_confidence(level: &ConfidenceLevel) -> ConfidenceLevel {
    match level {
        ConfidenceLevel::High => ConfidenceLevel::Medium,
        ConfidenceLevel::Medium | ConfidenceLevel::Low => ConfidenceLevel::Low,
    }
}

fn confidence_rank(level: &ConfidenceLevel) -> u8 {
    match level {
        ConfidenceLevel::High => 2,
        ConfidenceLevel::Medium => 1,
        ConfidenceLevel::Low => 0,
    }
}

fn normalize_subject(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn is_internal_path(path: &MemoryPath) -> bool {
    matches!(path.as_str(), "MEMORY.md" | "_schema.md" | "_log.md")
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use moa_core::{ConfidenceLevel, MemoryScope, MemoryStore, PageType, WikiPage};
    use tempfile::tempdir;

    use super::{normalize_relative_dates, run_consolidation};
    use crate::FileMemoryStore;

    fn page(content: &str, page_type: PageType) -> WikiPage {
        let now = Utc.with_ymd_and_hms(2026, 4, 9, 12, 0, 0).unwrap();
        WikiPage {
            path: None,
            title: "Page".to_string(),
            page_type,
            content: content.to_string(),
            created: now,
            updated: now - Duration::days(45),
            confidence: ConfidenceLevel::High,
            related: Vec::new(),
            sources: Vec::new(),
            tags: Vec::new(),
            auto_generated: false,
            last_referenced: now - Duration::days(45),
            reference_count: 0,
            metadata: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn relative_dates_normalize_to_absolute_dates() {
        let now = Utc.with_ymd_and_hms(2026, 4, 9, 12, 0, 0).unwrap();
        let (normalized, replacements) =
            normalize_relative_dates("Ship it today, revisit next week.", now);

        assert_eq!(replacements, 2);
        assert!(normalized.contains("2026-04-09"));
        assert!(normalized.contains("2026-04-16"));
    }

    #[tokio::test]
    async fn consolidation_resolves_dates_prunes_and_refreshes_index() {
        let dir = tempdir().unwrap();
        let store = FileMemoryStore::new(dir.path()).await.unwrap();
        let scope = MemoryScope::Workspace {
            workspace_id: "ws1".into(),
        };

        store
            .write_page(
                &scope,
                &"topics/architecture.md".into(),
                page(
                    "# Architecture\n\nAuth service runs on port 3000 today.",
                    PageType::Topic,
                ),
            )
            .await
            .unwrap();

        let mut conflicting = page(
            "# Deployment\n\nAuth service runs on port 4000.",
            PageType::Topic,
        );
        conflicting.reference_count = 1;
        store
            .write_page(&scope, &"topics/deployment.md".into(), conflicting)
            .await
            .unwrap();

        let mut stale = page("# Retired Service\n\nRetired.", PageType::Entity);
        stale
            .metadata
            .insert("entity_exists".to_string(), serde_json::Value::Bool(false));
        store
            .write_page(&scope, &"entities/retired-service.md".into(), stale)
            .await
            .unwrap();

        let report = run_consolidation(&store, &scope).await.unwrap();

        assert!(report.relative_dates_normalized >= 1);
        assert!(report.contradictions_resolved >= 1);
        assert_eq!(report.pages_deleted, 1);
        let deployment = store
            .read_page(&scope, &"topics/deployment.md".into())
            .await
            .unwrap();
        assert!(deployment.content.contains("port 3000"));
        assert!(
            store
                .read_page(&scope, &"entities/retired-service.md".into())
                .await
                .is_err()
        );
        assert!(report.memory_lines_after <= 200);
    }
}

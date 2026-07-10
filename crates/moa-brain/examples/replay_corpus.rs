//! Golden replay corpus: offline before/after cost comparison for history compilation.
//!
//! History compilation is a pure function of `(event log, config)`, so replaying
//! a fixed corpus of session event logs on two checkouts and diffing the reports
//! measures exactly what a context-pipeline change does to compiled tokens,
//! cross-turn byte stability, and projected input cost — no provider calls.
//!
//! Usage (requires the compose Postgres for store construction only; the store
//! is never read during replay):
//!
//! ```text
//! cargo run -p moa-brain --features eval-harness --example replay_corpus -- synthesize <dir>
//! cargo run -p moa-brain --features eval-harness --example replay_corpus -- replay <dir> [--json]
//! ```
//!
//! `synthesize` writes deterministic scenario logs (fixed UUIDs and timestamps)
//! so the corpus can be committed or regenerated identically. `replay` compiles
//! every turn of every scenario and reports, per scenario: compiled tokens,
//! the byte-shared prefix ratio between consecutive turns (the span a provider
//! prompt cache can serve), and projected input cost in cents under the
//! reference Sonnet pricing (fresh 3.0 / cached 0.3 dollars per MTok).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_brain::pipeline::history::HistoryCompiler;
use moa_core::{
    ContextMessage, Event, EventRecord, ModelId, ModelTier, Result, SessionId, ToolCallId,
    ToolOutput, estimate_text_tokens,
};
use moa_session::testing;
use serde::Serialize;
use serde_json::json;

/// Per-turn compile budget used by this offline replay-cost example.
const TURN_BUDGET: usize = 160_000;
/// Reference input price for uncached tokens, dollars per MTok.
const FRESH_PER_MTOK: f64 = 3.0;
/// Reference input price for cache-read tokens, dollars per MTok.
const CACHED_PER_MTOK: f64 = 0.3;

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    match args.get(1).map(String::as_str) {
        Some("synthesize") => {
            let dir = PathBuf::from(args.get(2).map(String::as_str).unwrap_or("replay-corpus"));
            synthesize(&dir)?;
            println!("wrote corpus scenarios to {}", dir.display());
            Ok(())
        }
        Some("replay") => {
            let dir = PathBuf::from(args.get(2).map(String::as_str).unwrap_or("replay-corpus"));
            let as_json = args.iter().any(|arg| arg == "--json");
            replay(&dir, as_json).await
        }
        _ => {
            eprintln!("usage: replay_corpus synthesize <dir> | replay <dir> [--json]");
            std::process::exit(2);
        }
    }
}

/// One scenario's replay metrics, stable-ordered for cross-checkout diffing.
#[derive(Debug, Serialize)]
struct ScenarioReport {
    scenario: String,
    turns: usize,
    /// Sum of compiled history tokens across every turn's compile.
    total_compiled_tokens: usize,
    /// Tokens of the final turn's compiled history.
    final_turn_tokens: usize,
    /// Sum over turns of tokens in the byte-shared prefix with the prior turn
    /// — the span a provider prompt cache can serve.
    total_shared_prefix_tokens: usize,
    /// Sum over turns of prior-turn tokens past the divergence point.
    total_invalidated_tokens: usize,
    /// Mean shared-prefix fraction across turns 2..N.
    mean_shared_prefix_ratio: f64,
    /// Compiled messages containing a dedup pointer or stale placeholder.
    placeholder_messages: usize,
    /// Projected input cost in cents across all turns (shared prefix billed
    /// at the cached rate, the remainder at the fresh rate).
    projected_input_cost_cents: f64,
}

async fn replay(dir: &Path, as_json: bool) -> Result<()> {
    // The store satisfies `HistoryCompiler::new` only; `compile_messages`
    // never touches it during replay.
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await?;
    let compiler = HistoryCompiler::new(Arc::new(store));

    let mut paths = std::fs::read_dir(dir)
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    paths.sort();

    let mut reports = Vec::new();
    for path in &paths {
        let raw = std::fs::read_to_string(path)
            .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
        let events: Vec<EventRecord> = serde_json::from_str(&raw)?;
        reports.push(replay_scenario(&compiler, path, &events)?);
    }

    if as_json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
        return Ok(());
    }

    println!(
        "{:<28} {:>5} {:>10} {:>10} {:>12} {:>12} {:>7} {:>6} {:>10}",
        "scenario",
        "turns",
        "compiled",
        "final",
        "shared",
        "invalidated",
        "shr%",
        "phold",
        "cost(¢)"
    );
    for report in &reports {
        println!(
            "{:<28} {:>5} {:>10} {:>10} {:>12} {:>12} {:>6.1}% {:>6} {:>10.3}",
            report.scenario,
            report.turns,
            report.total_compiled_tokens,
            report.final_turn_tokens,
            report.total_shared_prefix_tokens,
            report.total_invalidated_tokens,
            report.mean_shared_prefix_ratio * 100.0,
            report.placeholder_messages,
            report.projected_input_cost_cents,
        );
    }
    Ok(())
}

/// Replays one scenario, compiling history at every user-turn boundary.
fn replay_scenario(
    compiler: &HistoryCompiler,
    path: &Path,
    events: &[EventRecord],
) -> Result<ScenarioReport> {
    let scenario = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("scenario")
        .to_string();

    let turn_boundaries = events
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(record.event, Event::UserMessage { .. }).then_some(index)
        })
        .collect::<Vec<_>>();

    let mut previous: Vec<ContextMessage> = Vec::new();
    let mut total_compiled_tokens = 0usize;
    let mut final_turn_tokens = 0usize;
    let mut total_shared = 0usize;
    let mut total_invalidated = 0usize;
    let mut shared_ratios = Vec::new();
    let mut placeholder_messages = 0usize;
    let mut projected_cost_cents = 0f64;

    for boundary in &turn_boundaries {
        let (messages, tokens_used) =
            compiler.compile_messages(&events[..=*boundary], TURN_BUDGET)?;
        total_compiled_tokens += tokens_used;
        final_turn_tokens = tokens_used;

        let shared = previous
            .iter()
            .zip(messages.iter())
            .take_while(|(old, new)| old == new)
            .count();
        let shared_tokens = messages[..shared]
            .iter()
            .map(|message| estimate_text_tokens(&message.content))
            .sum::<usize>();
        let fresh_tokens = tokens_used.saturating_sub(shared_tokens);
        let invalidated_tokens = previous[shared.min(previous.len())..]
            .iter()
            .map(|message| estimate_text_tokens(&message.content))
            .sum::<usize>();

        if !previous.is_empty() {
            total_shared += shared_tokens;
            total_invalidated += invalidated_tokens;
            if tokens_used > 0 {
                shared_ratios.push(shared_tokens as f64 / tokens_used as f64);
            }
        }
        projected_cost_cents += (shared_tokens as f64 * CACHED_PER_MTOK
            + fresh_tokens as f64 * FRESH_PER_MTOK)
            / 1_000_000.0
            * 100.0;

        placeholder_messages = messages
            .iter()
            .filter(|message| {
                message.content.contains("[file previously read")
                    || message.content.contains("[file content unchanged")
            })
            .count();
        previous = messages;
    }

    let mean_shared_prefix_ratio = if shared_ratios.is_empty() {
        0.0
    } else {
        shared_ratios.iter().sum::<f64>() / shared_ratios.len() as f64
    };

    Ok(ScenarioReport {
        scenario,
        turns: turn_boundaries.len(),
        total_compiled_tokens,
        final_turn_tokens,
        total_shared_prefix_tokens: total_shared,
        total_invalidated_tokens: total_invalidated,
        mean_shared_prefix_ratio,
        placeholder_messages,
        projected_input_cost_cents: projected_cost_cents,
    })
}

/// Deterministic event factory: fixed session id, sequential UUIDs, and a
/// fixed epoch so synthesized corpora are byte-identical across runs.
struct EventFactory {
    session_id: SessionId,
    sequence: u64,
    events: Vec<EventRecord>,
}

impl EventFactory {
    fn new(seed: u128) -> Self {
        Self {
            session_id: SessionId(uuid::Uuid::from_u128(seed)),
            sequence: 0,
            events: Vec::new(),
        }
    }

    fn push(&mut self, event: Event) {
        let sequence = self.sequence;
        self.sequence += 1;
        self.events.push(EventRecord {
            id: uuid::Uuid::from_u128(0x1000_0000 + u128::from(sequence)),
            session_id: self.session_id,
            sequence_num: sequence,
            event_type: event.event_type(),
            event,
            timestamp: fixed_timestamp(sequence),
            brain_id: None,
            hand_id: None,
            token_count: None,
        });
    }

    fn user(&mut self, text: &str) {
        self.push(Event::UserMessage {
            text: text.to_string(),
            attachments: Vec::new(),
        });
    }

    fn assistant(&mut self, text: &str) {
        self.push(Event::BrainResponse {
            text: text.to_string(),
            thought_signature: None,
            model: ModelId::new("claude-sonnet-4-6"),
            model_tier: ModelTier::Main,
            input_tokens_uncached: 1,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 1,
            cost_cents: 0,
            duration_ms: 1,
        });
    }

    fn tool_exchange(&mut self, name: &str, input: serde_json::Value, output: &str) {
        let tool_id = ToolCallId(uuid::Uuid::from_u128(
            0x2000_0000 + u128::from(self.sequence),
        ));
        let provider_id = format!("toolu_{}", self.sequence);
        self.push(Event::ToolCall {
            tool_id,
            provider_tool_use_id: Some(provider_id.clone()),
            provider_thought_signature: None,
            tool_name: name.to_string(),
            input,
            hand_id: None,
        });
        self.push(Event::ToolResult {
            tool_id,
            provider_tool_use_id: Some(provider_id),
            output: ToolOutput::text(output, Duration::default()),
            original_output_tokens: None,
            success: true,
            duration_ms: 1,
        });
    }

    fn full_read(&mut self, path: &str, content: &str) {
        self.tool_exchange("file_read", json!({ "path": path }), content);
    }

    fn checkpoint(&mut self, summary: &str, events_summarized: u64) {
        self.push(Event::Checkpoint {
            summary: summary.to_string(),
            events_summarized,
            token_count: estimate_text_tokens(summary),
            model: ModelId::new("claude-sonnet-4-6"),
            model_tier: ModelTier::Auxiliary,
            input_tokens: 10,
            output_tokens: 5,
            cost_cents: 0,
        });
    }
}

fn fixed_timestamp(sequence: u64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_750_000_000 + sequence as i64, 0).unwrap_or_else(Utc::now)
}

fn file_body(path: &str, version: u32) -> String {
    (1..=120)
        .map(|line| format!("{path}-v{version}-line{line}: {}\n", "x".repeat(40)))
        .collect()
}

fn bash_output(turn: usize) -> String {
    (1..=80)
        .map(|line| format!("bash-t{turn}-line{line}: {}\n", "y".repeat(60)))
        .collect()
}

fn synthesize(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|error| moa_core::MoaError::StorageError(format!("create corpus dir: {error}")))?;

    // Scenario 1: a coding loop that re-reads the same files with unchanged
    // content — the dedup pointer path.
    let mut factory = EventFactory::new(1);
    for turn in 0..10 {
        factory.user(&format!("turn {turn}: inspect the auth module"));
        let path = ["src/auth.rs", "src/session.rs"][turn % 2];
        factory.full_read(path, &file_body(path, 1));
        factory.assistant(&format!("turn {turn} findings noted."));
    }
    write_scenario(dir, "reread_identical", &factory.events)?;

    // Scenario 2: edits between re-reads — changed content stays frozen until
    // the mid-session checkpoint opens the stale-placeholder gate.
    let mut factory = EventFactory::new(2);
    for turn in 0..10 {
        factory.user(&format!("turn {turn}: fix and re-check the parser"));
        factory.full_read("src/parser.rs", &file_body("src/parser.rs", turn as u32));
        factory.assistant(&format!("turn {turn} patch applied."));
        if turn == 6 {
            factory.checkpoint("parser work summarized through turn 2", 9);
        }
    }
    write_scenario(dir, "reread_changed_checkpoint", &factory.events)?;

    // Scenario 3: tool-heavy session with large bash/grep outputs and no
    // re-reads — measures raw replay volume and budget behavior.
    let mut factory = EventFactory::new(3);
    for turn in 0..12 {
        factory.user(&format!("turn {turn}: run the diagnostics"));
        factory.tool_exchange(
            "bash",
            json!({ "cmd": format!("diagnose --stage {turn}") }),
            &bash_output(turn),
        );
        factory.tool_exchange(
            "grep",
            json!({ "pattern": format!("stage-{turn}"), "path": "." }),
            &format!("logs/run.log: stage-{turn} completed\n"),
        );
        factory.assistant(&format!("turn {turn} diagnostics reviewed."));
    }
    write_scenario(dir, "tool_heavy", &factory.events)?;

    Ok(())
}

fn write_scenario(dir: &Path, name: &str, events: &[EventRecord]) -> Result<()> {
    let path = dir.join(format!("{name}.json"));
    let payload = serde_json::to_string_pretty(events)?;
    std::fs::write(&path, payload)
        .map_err(|error| moa_core::MoaError::StorageError(format!("write {name}: {error}")))
}

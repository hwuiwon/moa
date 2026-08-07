//! Deterministic case study of MOA's grounded-facts regression oracle.
//!
//! Runs the *real* production code paths:
//!   - `moa_skills::evidence::sanitize_segment_evidence` (sanitization gate)
//!   - `moa_skills::regression::generate_skill_test_suite_source_for_name` (suite + oracle generation)
//!   - `moa_eval_core::assertion::evaluate_assertions` + `builtin_registry` (gate-time evaluation)
//! on synthetic task segments with planted, typed facts, then measures:
//!   E1: oracle selection (grounded vs keyword fallback), facts/case, fact types, determinism
//!   E1b: adversarial substring grounding (word-boundary property)
//!   E2: perturbation detection under the grounded-facts oracle
//!   E3: same perturbations under the keyword-fallback oracle (ablation)
//!   E4: action assertions (dropped tool, reordered/repeated route)

use std::collections::BTreeMap;
use std::time::Duration;

use moa_core::events::Event;
use moa_core::types::channel::{Attachment, Channel};
use moa_core::types::events_stream::EventRecord;
use moa_core::types::identifiers::{
    ModelId, SegmentId, SessionId, TenantId, ToolCallId,
};
use moa_core::types::provider::ModelTier;
use moa_core::types::security::{ToolCapabilityId, ToolOutputAssessment};
use moa_core::types::tools::ToolOutput;
use moa_eval_core::assertion::{builtin_registry, evaluate_assertions, AssertionOutcome};
use moa_eval_core::evidence::{ActionKind, ActionOutcome, EvidenceEnvelope, EvidenceSubject};
use moa_eval_core::types::TEST_CASE_SCHEMA_VERSION;
use moa_eval_core::{SuiteOracle, TestCase, TestSuite};
use moa_memory_pii::HeuristicPiiClassifier;
use moa_skills::evidence::{sanitize_segment_evidence, EvidenceScope, SegmentNarrative};
use moa_skills::regression::generate_skill_test_suite_source_for_name;
use uuid::Uuid;

// ---------- deterministic RNG (SplitMix64) ----------
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ---------- planted facts ----------
#[derive(Clone, Debug)]
struct Fact {
    kind: &'static str,
    text: String,
}

fn make_facts(rng: &mut Rng, i: usize) -> Vec<Fact> {
    let uuid = Uuid::from_u128(((rng.next() as u128) << 64) | rng.next() as u128);
    vec![
        Fact { kind: "number_unit", text: format!("{}ms", 100 + rng.below(900)) },
        Fact { kind: "currency", text: format!("${},{:03}.{:02}", 1 + rng.below(9), rng.below(1000), rng.below(100)) },
        Fact { kind: "percent", text: format!("{}%", 3 + rng.below(96)) },
        Fact { kind: "uuid", text: uuid.to_string() },
        Fact { kind: "ref_token", text: format!("MOA-{}", 100 + rng.below(900)) },
        Fact { kind: "path", text: format!("/srv/app/config/settings_{i}.yaml") },
        Fact { kind: "url", text: format!("https://api.example.com/v2/orders/{}", 1000 + rng.below(9000)) },
        Fact { kind: "identifier", text: format!("refresh_token_ttl_{}", rng.below(100)) },
    ]
}

const TOOL_POOL: [&str; 8] = [
    "bash", "file_search", "file_read", "grep", "http_get", "file_write", "sql_query", "kubectl",
];

const FILLER: [&str; 6] = [
    "checked the deployment logs for anomalies",
    "compared the staging configuration against production",
    "confirmed the rollout completed on every replica",
    "inspected the connection pool metrics afterwards",
    "reviewed the alert thresholds for the service",
    "validated the schema migration output carefully",
];

struct Segment {
    events: Vec<EventRecord>,
    session_id: SessionId,
    tenant_id: TenantId,
    carried: Vec<Fact>,
    distinct_tools: Vec<String>,
    ordered_tools: Vec<String>,
    response: String,
}

fn push_event(events: &mut Vec<EventRecord>, session_id: SessionId, event: Event) {
    events.push(EventRecord {
        id: Uuid::now_v7(),
        session_id,
        sequence_num: events.len() as u64 + 1,
        event_type: event.event_type(),
        event,
        timestamp: chrono::Utc::now(),
        brain_id: None,
        hand_id: None,
        token_count: None,
    });
}

/// Builds a synthetic successful segment with `t` tool calls whose results embed
/// all facts, and a final response carrying `k` of those facts verbatim.
/// When `groundable` is false, tool outputs are generic so nothing grounds.
fn build_segment(rng: &mut Rng, i: usize, t: usize, k: usize, groundable: bool) -> Segment {
    let facts = make_facts(rng, i);
    let session_id = SessionId(Uuid::from_u128(0x018f_1a30_0000_7000_8000_0000_0000_0000 + i as u128));
    let tenant_id = TenantId::from(Uuid::from_u128(0x42));
    let mut events = Vec::new();

    push_event(&mut events, session_id, Event::UserMessage {
        text: format!("investigate the orders api latency regression run {i}"),
        attachments: Vec::<Attachment>::new(),
    });

    let mut ordered_tools = Vec::new();
    for j in 0..t {
        let tool_name = TOOL_POOL[(i + j) % TOOL_POOL.len()].to_string();
        ordered_tools.push(tool_name.clone());
        // Spread every fact across the tool outputs so carried facts ground.
        let fact = &facts[j % facts.len()];
        let extra = &facts[(j + 3) % facts.len()];
        let output = if groundable {
            format!(
                "step {j} ok: observed {} and {} while {}",
                fact.text, extra.text, FILLER[j % FILLER.len()]
            )
        } else {
            // No digits or identifier-shaped tokens: nothing in the response can ground.
            format!("tool step ok - {}", FILLER[j % FILLER.len()])
        };
        let tool_id = ToolCallId::new();
        push_event(&mut events, session_id, Event::ToolCall {
            tool_id,
            provider_tool_use_id: None,
            provider_thought_signature: None,
            tool_name: tool_name.clone(),
            input: serde_json::json!({"arg": format!("probe-{j}")}),
            hand_id: None,
        });
        push_event(&mut events, session_id, Event::ToolResult {
            tool_id,
            provider_tool_use_id: None,
            output: ToolOutput::text(output, Duration::from_millis(1)),
            original_output_tokens: None,
            success: true,
            duration_ms: 1,
            assessment: ToolOutputAssessment::safe(),
            capability: ToolCapabilityId::builtin("bash"),
        });
    }

    let carried: Vec<Fact> = facts.iter().take(k).cloned().collect();
    let mut response = String::from("Investigation complete. ");
    for (idx, f) in carried.iter().enumerate() {
        response.push_str(&format!(
            "Finding {}: {} ({}). ",
            idx + 1,
            f.text,
            FILLER[idx % FILLER.len()]
        ));
    }
    response.push_str("The service is healthy again and monitoring stays enabled.");

    push_event(&mut events, session_id, Event::BrainResponse {
        text: response.clone(),
        thought_signature: None,
        model: ModelId::new("scripted-skill-model"),
        model_tier: ModelTier::Auxiliary,
        input_tokens_uncached: 128,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens: 32,
        cost_cents: 0,
        duration_ms: 1,
        llm_ttft_ms: None,
    });

    let mut distinct_tools = ordered_tools.clone();
    distinct_tools.sort();
    distinct_tools.dedup();

    Segment { events, session_id, tenant_id, carried, distinct_tools, ordered_tools, response }
}

async fn generate_suite(seg: &Segment) -> Option<TestSuite> {
    let scope = EvidenceScope {
        tenant_id: seg.tenant_id,
        contact_id: None,
        session_id: seg.session_id,
        segment_id: SegmentId::new(),
        experience_id: Uuid::now_v7(),
    };
    let narrative = SegmentNarrative {
        task_summary: Some("investigate orders api latency regression"),
        assessment_summaries: &["verification tool run passed".to_string()],
    };
    let evidence = sanitize_segment_evidence(&HeuristicPiiClassifier, scope, &seg.events, narrative)
        .await
        .ok()?;
    let generated =
        generate_skill_test_suite_source_for_name(seg.tenant_id, "oracle-study-skill", &evidence)
            .ok()?;
    toml::from_str::<TestSuite>(&generated.source_toml).ok()
}

fn eval_case(case: &TestCase, response: &str, tools: &[String]) -> Vec<AssertionOutcome> {
    let mut builder = EvidenceEnvelope::builder(EvidenceSubject {
        case: case.name.clone(),
        case_schema_version: TEST_CASE_SCHEMA_VERSION,
        agent_config: "study".to_string(),
        run_label: "study".to_string(),
    })
    .source("oracle_study")
    .response(response.to_string());
    for tool in tools {
        builder = builder.action(
            ActionKind::Invocation,
            tool.clone(),
            serde_json::Value::Null,
            ActionOutcome::Succeeded,
        );
    }
    evaluate_assertions(builtin_registry(), case, Some(&builder.build()))
}

fn gate_fails(outcomes: &[AssertionOutcome]) -> bool {
    outcomes.iter().any(AssertionOutcome::is_gate_failure)
}

/// Mutates every digit in fact occurrences within the response (fabrication).
fn fabricate(response: &str, carried: &[Fact]) -> String {
    let mut out = response.to_string();
    for f in carried {
        let mutated: String = f.text.chars().map(|c| match c {
            '0'..='8' => char::from_digit(c.to_digit(10).unwrap() + 1, 10).unwrap(),
            '9' => '3',
            c => c,
        }).collect();
        out = out.replace(&f.text, &mutated);
    }
    out
}

fn omit(response: &str, carried: &[Fact]) -> String {
    let mut out = response.to_string();
    for f in carried {
        out = out.replace(&f.text, "the recorded value");
    }
    out
}

fn paraphrase(seg: &Segment) -> String {
    let mut r = String::from("Work finished without further incident. ");
    for f in &seg.carried {
        r.push_str(&format!("We recorded {} during the run. ", f.text));
    }
    r.push_str("No further action required for this maintenance window.");
    r
}

#[derive(Default, Debug)]
struct Counter(BTreeMap<String, usize>);
impl Counter {
    fn add(&mut self, key: &str) {
        *self.0.entry(key.to_string()).or_default() += 1;
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut results = serde_json::Map::new();

    // ---------------- E1: oracle characterization ----------------
    let mut rng = Rng(7);
    let n1 = 300usize;
    let mut oracle_kinds = Counter::default();
    let mut facts_hist = Counter::default();
    let mut kind_by_k: BTreeMap<usize, Counter> = BTreeMap::new();
    let mut fact_type_counts = Counter::default();
    let mut determinism_ok = 0usize;
    for i in 0..n1 {
        let t = 2 + (i % 7); // 2..=8 tool calls
        let k = i % 6; // 0..=5 carried facts
        let seg = build_segment(&mut rng, i, t, k, true);
        let suite = generate_suite(&seg).await.expect("suite");
        let suite2 = generate_suite(&seg).await.expect("suite2");
        let case = &suite.cases[0];
        let case2 = &suite2.cases[0];
        if case.assertions == case2.assertions && case.oracle == case2.oracle {
            determinism_ok += 1;
        }
        let oracle = case.oracle.clone();
        let label = match oracle {
            Some(SuiteOracle::GroundedFacts) => "grounded_facts",
            Some(SuiteOracle::Keywords) => "keywords",
            None => "none",
        };
        oracle_kinds.add(label);
        kind_by_k.entry(k).or_default().add(label);
        // Count asserted facts from the text-match assertion config.
        if let Some(spec) = case.assertions.iter().find(|a| a.id == "response-carries-source-facts") {
            let contains = spec.config.get("contains").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            facts_hist.add(&format!("{label}:{contains}"));
            if label == "grounded_facts" {
                if let Some(arr) = spec.config.get("contains").and_then(|v| v.as_array()) {
                    for v in arr {
                        let s = v.as_str().unwrap_or("");
                        let kind = seg
                            .carried
                            .iter()
                            .find(|f| f.text.eq_ignore_ascii_case(s))
                            .map(|f| f.kind)
                            .unwrap_or("other");
                        fact_type_counts.add(kind);
                    }
                }
            }
        }
    }
    results.insert("e1_oracle_kinds".into(), serde_json::to_value(&oracle_kinds.0).unwrap());
    results.insert("e1_by_carried_facts".into(), serde_json::to_value(
        kind_by_k.iter().map(|(k, c)| (k.to_string(), c.0.clone())).collect::<BTreeMap<_, _>>()
    ).unwrap());
    results.insert("e1_facts_per_case".into(), serde_json::to_value(&facts_hist.0).unwrap());
    results.insert("e1_fact_types".into(), serde_json::to_value(&fact_type_counts.0).unwrap());
    results.insert("e1_determinism".into(), serde_json::json!({"identical": determinism_ok, "total": n1}));

    // ---------------- E1b: adversarial substring grounding ----------------
    // Response fact "42..." appears only as a strict substring of a longer token
    // in tool output ("1{fact}"); word-boundary grounding must refuse it.
    let mut rng_b = Rng(11);
    let mut false_grounded = 0usize;
    let n1b = 200usize;
    for i in 0..n1b {
        let mut seg = build_segment(&mut rng_b, i, 3, 2, false);
        // Overwrite one tool result with superstring-corrupted fact text.
        let carried = seg.carried.clone();
        for record in &mut seg.events {
            if let Event::ToolResult { output, .. } = &mut record.event {
                let corrupted = carried
                    .iter()
                    .map(|f| format!("1{}x", f.text))
                    .collect::<Vec<_>>()
                    .join(" ");
                *output = ToolOutput::text(format!("observed {corrupted}"), Duration::from_millis(1));
            }
        }
        let suite = generate_suite(&seg).await.expect("suite");
        let case = &suite.cases[0];
        if matches!(case.oracle, Some(SuiteOracle::GroundedFacts)) {
            // Any grounded fact equal to a carried fact would be a false grounding.
            if let Some(spec) = case.assertions.iter().find(|a| a.id == "response-carries-source-facts") {
                if let Some(arr) = spec.config.get("contains").and_then(|v| v.as_array()) {
                    if arr.iter().any(|v| carried.iter().any(|f| v.as_str() == Some(f.text.as_str()))) {
                        false_grounded += 1;
                    }
                }
            }
        }
    }
    results.insert("e1b_substring_false_grounding".into(), serde_json::json!({
        "false_grounded": false_grounded, "total": n1b
    }));

    // ---------------- E1c: punctuation-extension adversarial grounding ----------------
    // The response asserts fact F; the tool corpus contains ONLY an extended
    // variant F' where the extension begins with a NON-word character
    // ('.', '-', '/'), so ASCII word boundaries hold on both sides of the
    // F-occurrence inside F'. A grounding here corroborates a fact the tool
    // never actually reported (e.g. response "$1,200" vs tool "$1,200.99").
    let mut rng_c = Rng(47);
    let n1c = 300usize;
    let mut e1c_by_type: BTreeMap<&str, (usize, usize)> = BTreeMap::new(); // (false_grounded, total)
    for i in 0..n1c {
        let variant = i % 6;
        let a = 100 + rng_c.below(900);
        let b = rng_c.below(90) + 10;
        let (kind, fact, extended): (&str, String, String) = match variant {
            0 => ("currency_cents", format!("${},{:03}", 1 + rng_c.below(9), rng_c.below(1000)),
                  String::new()),
            1 => ("ref_token_suffix", format!("ABC-{a}"), String::new()),
            2 => ("path_prefix", format!("/tmp/foo_{a}"), String::new()),
            3 => ("url_prefix", format!("https://api.example.com/v2/x{a}"), String::new()),
            4 => ("bare_number_decimal", format!("{a}"), String::new()),
            _ => ("number_unit_hyphen", format!("{a}ms"), String::new()),
        };
        let extended = if !extended.is_empty() { extended } else {
            match variant {
                0 => format!("{fact}.{b:02}"),      // $1,200 vs $1,200.99
                1 => format!("{fact}-{b}"),          // ABC-123 vs ABC-123-456
                2 => format!("{fact}/bar"),          // /tmp/foo vs /tmp/foo/bar
                3 => format!("{fact}/y"),            // https://a.com/x vs .../x/y
                4 => format!("{fact}.{}", rng_c.below(9) + 1), // 12 vs 12.5
                _ => format!("{fact}-related"),      // 412ms vs 412ms-related
            }
        };
        let mut seg = build_segment(&mut rng_c, i + 20_000, 3, 0, false);
        // Response asserts exactly this fact; corpus carries only the extension.
        let response = format!("Confirmed the reported value {fact} for this run.");
        for record in &mut seg.events {
            match &mut record.event {
                Event::ToolResult { output, .. } => {
                    *output = ToolOutput::text(
                        format!("tool observed {extended} during the check"),
                        Duration::from_millis(1),
                    );
                }
                Event::BrainResponse { text, .. } => *text = response.clone(),
                _ => {}
            }
        }
        let suite = generate_suite(&seg).await.expect("suite");
        let case = &suite.cases[0];
        let mut grounded_this_fact = false;
        if matches!(case.oracle, Some(SuiteOracle::GroundedFacts)) {
            if let Some(spec) = case.assertions.iter().find(|a| a.id == "response-carries-source-facts") {
                if let Some(arr) = spec.config.get("contains").and_then(|v| v.as_array()) {
                    grounded_this_fact = arr.iter().any(|v| v.as_str() == Some(fact.as_str()));
                }
            }
        }
        let e = e1c_by_type.entry(kind).or_insert((0, 0));
        e.0 += grounded_this_fact as usize;
        e.1 += 1;
    }
    results.insert("e1c_punctuation_extension_false_grounding".into(), serde_json::to_value(
        e1c_by_type.iter().map(|(k, (f, t))| (k.to_string(), serde_json::json!({"false_grounded": f, "total": t}))).collect::<BTreeMap<_, _>>()
    ).unwrap());

    // ---------------- E2 + E3: perturbation detection, grounded vs keyword ----------------
    let mut rng2 = Rng(23);
    let n2 = 200usize;
    let mut grounded_detect = BTreeMap::<&str, (usize, usize)>::new(); // (fail, total)
    let mut keyword_detect = BTreeMap::<&str, (usize, usize)>::new();
    for i in 0..n2 {
        let t = 3 + (i % 5);
        let k = 2 + (i % 4); // 2..=5 carried facts so grounding always possible
        let seg = build_segment(&mut rng2, i, t, k, true);
        let seg_kw = build_segment(&mut rng2, i + 10_000, t, k, false); // keyword-fallback twin
        let suite_g = generate_suite(&seg).await.expect("suite");
        let suite_k = generate_suite(&seg_kw).await.expect("suite");
        let case_g = &suite_g.cases[0];
        let case_k = &suite_k.cases[0];
        assert!(matches!(case_g.oracle, Some(SuiteOracle::GroundedFacts)));
        if !matches!(case_k.oracle, Some(SuiteOracle::Keywords)) {
            eprintln!("UNEXPECTED oracle for keyword twin {i}:\n{}", serde_json::to_string_pretty(&case_k.assertions).unwrap());
            eprintln!("response: {}", seg_kw.response);
            std::process::exit(2);
        }

        let variants: Vec<(&str, String, Vec<String>)> = vec![
            ("exact", seg.response.clone(), seg.ordered_tools.clone()),
            ("paraphrase_keep_facts", paraphrase(&seg), seg.ordered_tools.clone()),
            ("fabricate_facts", fabricate(&seg.response, &seg.carried), seg.ordered_tools.clone()),
            ("omit_facts", omit(&seg.response, &seg.carried), seg.ordered_tools.clone()),
        ];
        for (name, response, tools) in &variants {
            let fails_g = gate_fails(&eval_case(case_g, response, tools));
            if *name == "paraphrase_keep_facts" && fails_g && std::env::var("DBG").is_ok() {
                if let Some(spec) = case_g.assertions.iter().find(|a| a.id == "response-carries-source-facts") {
                    eprintln!("PARA_FAIL {i} expected={} carried={:?}", spec.config.get("contains").unwrap(), seg.carried.iter().map(|f| f.text.clone()).collect::<Vec<_>>());
                }
            }
            let e = grounded_detect.entry(name).or_insert((0, 0));
            e.0 += fails_g as usize;
            e.1 += 1;
        }
        // Keyword twin: same perturbation semantics applied to its own response.
        let kw_variants: Vec<(&str, String)> = vec![
            ("exact", seg_kw.response.clone()),
            ("paraphrase_keep_facts", paraphrase(&seg_kw)),
            ("fabricate_facts", fabricate(&seg_kw.response, &seg_kw.carried)),
            ("omit_facts", omit(&seg_kw.response, &seg_kw.carried)),
        ];
        for (name, response) in &kw_variants {
            let fails_k = gate_fails(&eval_case(case_k, response, &seg_kw.ordered_tools));
            let e = keyword_detect.entry(name).or_insert((0, 0));
            e.0 += fails_k as usize;
            e.1 += 1;
        }
    }
    let fmt = |m: &BTreeMap<&str, (usize, usize)>| -> serde_json::Value {
        serde_json::to_value(m.iter().map(|(k, (f, t))| (k.to_string(), serde_json::json!({"gate_failures": f, "total": t}))).collect::<BTreeMap<_, _>>()).unwrap()
    };
    results.insert("e2_grounded_oracle".into(), fmt(&grounded_detect));
    results.insert("e3_keyword_oracle".into(), fmt(&keyword_detect));

    // ---------------- E4: action assertions ----------------
    let mut rng4 = Rng(31);
    let n4 = 200usize;
    let mut e4 = BTreeMap::<&str, (usize, usize)>::new();
    for i in 0..n4 {
        let t = 3 + (i % 5);
        let seg = build_segment(&mut rng4, i, t, 3, true);
        let suite = generate_suite(&seg).await.expect("suite");
        let case = &suite.cases[0];
        // dropped tool: remove one distinct tool entirely
        let dropped: Vec<String> = seg
            .ordered_tools
            .iter()
            .filter(|x| **x != seg.distinct_tools[0])
            .cloned()
            .collect();
        // rerouted: reversed order plus duplicated first call, same distinct set
        let mut rerouted: Vec<String> = seg.ordered_tools.iter().rev().cloned().collect();
        rerouted.push(seg.ordered_tools[0].clone());
        let variants: Vec<(&str, &Vec<String>)> = vec![
            ("dropped_tool", &dropped),
            ("reordered_and_repeated", &rerouted),
        ];
        for (name, tools) in variants {
            let fails = gate_fails(&eval_case(case, &seg.response, tools));
            let e = e4.entry(name).or_insert((0, 0));
            e.0 += fails as usize;
            e.1 += 1;
        }
    }
    results.insert("e4_action_assertions".into(), fmt(&e4));

    println!("{}", serde_json::to_string_pretty(&serde_json::Value::Object(results)).unwrap());
}

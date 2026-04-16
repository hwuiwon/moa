# 05 — Session & Event Log

_Postgres event schema, compaction, Temporal integration, replay._

---

## Storage: Postgres

Same dialect everywhere. Local development uses Docker Compose with Postgres 18 + pgvector on
`localhost:5432`. Cloud deployments use managed Postgres / Neon.

Crate: `sqlx` with the Postgres driver.

---

## Schema

```sql
-- Sessions
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,                    -- UUID
    workspace_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    title TEXT,                             -- auto-generated or user-set
    status TEXT NOT NULL DEFAULT 'created', -- created|running|paused|waiting_approval|completed|cancelled|failed
    platform TEXT,                          -- telegram|slack|discord|desktop|cli
    platform_channel TEXT,                  -- platform-specific channel/thread ID
    model TEXT,                             -- model used for this session
    created_at TEXT NOT NULL,               -- ISO 8601
    updated_at TEXT NOT NULL,
    completed_at TEXT,
    parent_session_id TEXT,                 -- for sub-brain sessions
    total_input_tokens INTEGER DEFAULT 0,
    total_output_tokens INTEGER DEFAULT 0,
    total_cost_cents INTEGER DEFAULT 0,     -- in cents to avoid float
    event_count INTEGER DEFAULT 0,
    last_checkpoint_seq INTEGER,            -- sequence of last checkpoint
    FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
);

CREATE INDEX idx_sessions_workspace ON sessions(workspace_id, updated_at DESC);
CREATE INDEX idx_sessions_user ON sessions(user_id, updated_at DESC);
CREATE INDEX idx_sessions_status ON sessions(status);

-- Events (append-only)
CREATE TABLE events (
    id TEXT PRIMARY KEY,                    -- UUID
    session_id TEXT NOT NULL,
    sequence_num INTEGER NOT NULL,          -- monotonic per session
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,                  -- JSON
    timestamp TEXT NOT NULL,                -- ISO 8601
    brain_id TEXT,                          -- which brain emitted
    hand_id TEXT,                           -- which hand was involved
    token_count INTEGER,                    -- tokens consumed by this event
    UNIQUE(session_id, sequence_num),
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE INDEX idx_events_session_seq ON events(session_id, sequence_num);
CREATE INDEX idx_events_session_type ON events(session_id, event_type);
CREATE INDEX idx_events_timestamp ON events(timestamp);

-- FTS over events for cross-session search
CREATE VIRTUAL TABLE events_fts USING fts5(
    session_id,
    event_type,
    payload,
    content=events,
    content_rowid=rowid,
    tokenize='porter unicode61'
);

-- Approval rules (persistent per-workspace)
CREATE TABLE approval_rules (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    pattern TEXT NOT NULL,          -- glob pattern for arguments
    action TEXT NOT NULL,           -- allow | deny
    scope TEXT NOT NULL,            -- workspace | global
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(workspace_id, tool, pattern)
);

-- Workspace metadata
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT,                      -- filesystem path (local)
    created_at TEXT NOT NULL,
    last_active TEXT NOT NULL,
    session_count INTEGER DEFAULT 0
);

-- User metadata
CREATE TABLE users (
    id TEXT PRIMARY KEY,
    display_name TEXT,
    platform_links TEXT,            -- JSON: {"telegram": "123", "slack": "U456"}
    created_at TEXT NOT NULL,
    last_active TEXT NOT NULL
);
```

---

## Event types and payloads

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    // Session lifecycle
    SessionCreated { workspace_id: String, user_id: String, model: String },
    SessionStatusChanged { from: SessionStatus, to: SessionStatus },
    SessionCompleted { summary: String, total_turns: u32 },
    
    // User messages
    UserMessage { text: String, attachments: Vec<Attachment> },
    QueuedMessage { text: String, queued_at: DateTime<Utc> },
    
    // Brain output
    BrainThinking {
        summary: String,           // SHORT summary only (not full thinking tokens)
        token_count: usize,
    },
    BrainResponse {
        text: String,              // full response text
        model: String,
        input_tokens: usize,
        output_tokens: usize,
        cost_cents: u32,
        duration_ms: u64,
    },
    
    // Tool execution
    ToolCall {
        tool_id: Uuid,             // unique per call
        tool_name: String,
        input: serde_json::Value,  // full parameters
        hand_id: Option<String>,
    },
    ToolResult {
        tool_id: Uuid,             // matches ToolCall.tool_id
        output: String,            // full output
        success: bool,
        duration_ms: u64,
    },
    ToolError {
        tool_id: Uuid,
        error: String,
        retryable: bool,
    },
    
    // Approvals
    ApprovalRequested {
        request_id: Uuid,
        tool_name: String,
        input_summary: String,
        risk_level: RiskLevel,     // low | medium | high
    },
    ApprovalDecided {
        request_id: Uuid,
        decision: ApprovalDecision,
        decided_by: String,        // user ID
        decided_at: DateTime<Utc>,
    },
    
    // Memory operations
    MemoryRead { path: String, scope: String },
    MemoryWrite { path: String, scope: String, summary: String },
    
    // Hand lifecycle
    HandProvisioned { hand_id: String, provider: String, tier: String },
    HandDestroyed { hand_id: String, reason: String },
    HandError { hand_id: String, error: String },
    
    // Checkpoints (for compaction)
    Checkpoint {
        summary: String,           // LLM-generated summary of events since last checkpoint
        events_summarized: u64,    // how many events this checkpoint covers
        token_count: usize,        // tokens in the summary
    },
    
    // Errors
    Error { message: String, recoverable: bool },
    Warning { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApprovalDecision {
    AllowOnce,
    AlwaysAllow { pattern: String }, // stored as approval rule
    Deny { reason: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RiskLevel { Low, Medium, High }
```

---

## Core operations

### Replay and observation

Observation is history-first. Clients reconstruct a session from durable events in the store, then optionally attach a live in-memory tail from the active orchestrator.

Two implications follow from that contract:

- Losing the live tail must not silently lose information; callers can always reopen from durable history.
- If a live subscriber lags beyond the in-memory broadcast buffer, the stream should surface an error so the caller can reconnect from the last durable sequence it has seen.

### emit_event

```rust
impl PostgresSessionStore {
    pub async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<SequenceNum> {
        let event_id = Uuid::now_v7();
        let seq = self.next_sequence(session_id).await?;
        let payload = serde_json::to_string(&event)?;
        let now = Utc::now().to_rfc3339();
        
        sqlx::query(
            "INSERT INTO events (id, session_id, sequence_num, event_type, payload, timestamp, token_count)
             VALUES (?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(event_id.to_string())
        .bind(session_id.to_string())
        .bind(seq as i64)
        .bind(event.type_name())
        .bind(&payload)
        .bind(&now)
        .bind(event.token_count() as i64)
        .execute(&self.pool).await?;
        
        // Update session metadata
        sqlx::query(
            "UPDATE sessions SET 
                updated_at = ?,
                event_count = event_count + 1,
                total_input_tokens = total_input_tokens + ?,
                total_output_tokens = total_output_tokens + ?,
                total_cost_cents = total_cost_cents + ?
             WHERE id = ?"
        )
        .bind(&now)
        .bind(event.input_tokens() as i64)
        .bind(event.output_tokens() as i64)
        .bind(event.cost_cents() as i64)
        .bind(session_id.to_string())
        .execute(&self.pool).await?;
        
        // Update FTS index
        sqlx::query(
            "INSERT INTO events_fts (rowid, session_id, event_type, payload)
             VALUES (last_insert_rowid(), ?, ?, ?)"
        )
        .bind(session_id.to_string())
        .bind(event.type_name())
        .bind(&payload)
        .execute(&self.pool).await?;
        
        Ok(seq)
    }
}
```

### get_events (with range)

```rust
pub async fn get_events(
    &self,
    session_id: SessionId,
    range: EventRange,
) -> Result<Vec<EventRecord>> {
    let mut query = String::from(
        "SELECT id, session_id, sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count
         FROM events WHERE session_id = ?"
    );
    let mut binds = vec![session_id.to_string()];
    
    if let Some(from) = range.from_seq {
        query.push_str(" AND sequence_num >= ?");
        binds.push(from.to_string());
    }
    if let Some(to) = range.to_seq {
        query.push_str(" AND sequence_num <= ?");
        binds.push(to.to_string());
    }
    if let Some(types) = &range.event_types {
        let placeholders: Vec<&str> = types.iter().map(|_| "?").collect();
        query.push_str(&format!(" AND event_type IN ({})", placeholders.join(",")));
        for t in types {
            binds.push(t.to_string());
        }
    }
    
    query.push_str(" ORDER BY sequence_num ASC");
    
    if let Some(limit) = range.limit {
        query.push_str(&format!(" LIMIT {}", limit));
    }
    
    // Execute with dynamic binds...
    Ok(results)
}
```

### wake (recover brain from session log)

```rust
pub async fn wake(&self, session_id: SessionId) -> Result<WakeContext> {
    let session = self.get_session(session_id).await?;
    
    // Find the last checkpoint
    let last_checkpoint = sqlx::query_as::<_, EventRecord>(
        "SELECT * FROM events 
         WHERE session_id = ? AND event_type = 'Checkpoint'
         ORDER BY sequence_num DESC LIMIT 1"
    )
    .bind(session_id.to_string())
    .fetch_optional(&self.pool).await?;
    
    // Load events after the last checkpoint (or all if no checkpoint)
    let from_seq = last_checkpoint
        .as_ref()
        .map(|cp| cp.sequence_num + 1)
        .unwrap_or(0);
    
    let recent_events = self.get_events(session_id, EventRange {
        from_seq: Some(from_seq),
        to_seq: None,
        event_types: None,
        limit: None,
    }).await?;
    
    Ok(WakeContext {
        session,
        checkpoint_summary: last_checkpoint.map(|cp| cp.payload_as::<CheckpointData>()),
        recent_events,
    })
}
```

---

## Compaction

### Trigger

Compaction fires when:
- Event count since last checkpoint > 100 **OR**
- Estimated token usage of recent events > 70% of model's context window

### Process

```rust
async fn maybe_compact(
    store: &dyn SessionStore,
    llm: &dyn LLMProvider,
    session_id: SessionId,
    pipeline: &ContextPipeline,
) -> Result<bool> {
    let session = store.get_session(session_id).await?;
    let events_since_checkpoint = session.event_count - session.last_checkpoint_seq.unwrap_or(0);
    
    if events_since_checkpoint < 100 {
        return Ok(false); // not enough events
    }
    
    // Step 1: Memory flush — give agent a chance to save important facts
    let flush_events = store.get_events(session_id, EventRange::since_checkpoint()).await?;
    let flush_prompt = format!(
        "Before compacting context, review these recent events and save anything important \
         to memory. Focus on: errors encountered, decisions made, unresolved items, \
         and facts that should persist.\n\nEvents:\n{}",
        format_events_for_prompt(&flush_events)
    );
    
    let flush_response = llm.complete(CompletionRequest {
        messages: vec![ContextMessage::user(flush_prompt)],
        tools: vec![MemoryWriteTool::schema()], // only memory tool available
        ..Default::default()
    }).await?;
    
    // Execute any memory writes from the flush
    handle_tool_calls(&flush_response, session_id).await?;
    
    // Step 2: Generate checkpoint summary
    let summary_prompt = format!(
        "Summarize the following events into a concise checkpoint. Preserve:\n\
         - All errors encountered and their resolutions\n\
         - Architectural decisions made\n\
         - Unresolved items / next steps\n\
         - Active file paths being modified\n\
         - Key facts discovered\n\
         \nEvents:\n{}",
        format_events_for_prompt(&flush_events)
    );
    
    let summary = llm.complete(CompletionRequest::simple(summary_prompt)).await?;
    
    // Step 3: Emit checkpoint event
    store.emit_event(session_id, Event::Checkpoint {
        summary: summary.text,
        events_summarized: events_since_checkpoint as u64,
        token_count: summary.output_tokens,
    }).await?;
    
    Ok(true)
}
```

### What the HistoryCompiler (pipeline stage 6) does with checkpoints

```
If checkpoint exists:
  [Checkpoint summary] + [Last 5 turns verbatim]
  
If no checkpoint:
  [All events, most recent first]
  
Errors are ALWAYS preserved regardless of compaction.
```

---

## Temporal persistence model

Events persist in two places:

| Store | Purpose | Guarantees |
|---|---|---|
| Temporal event history | Crash recovery, workflow replay | Exactly-once, ordered |
| Postgres | Querying, observation, session replay, cross-session search | Durable, queryable source of truth |

The brain emits to Postgres within each Temporal activity. If the activity fails and retries, the Postgres write is idempotent (UNIQUE constraint on session_id + sequence_num).

In local mode (no Temporal), Postgres is still the only store. Crash recovery works by reading the last event and resuming from there.

---

## Cross-session search

```sql
-- Find sessions where we discussed OAuth
SELECT DISTINCT s.id, s.title, s.updated_at
FROM events_fts ef
JOIN events e ON e.rowid = ef.rowid
JOIN sessions s ON s.id = e.session_id
WHERE events_fts MATCH 'oauth token refresh'
  AND s.workspace_id = ?
ORDER BY s.updated_at DESC
LIMIT 10;
```

---

## Data retention

```toml
# ~/.moa/config.toml
[retention]
active_sessions_ttl = "90d"     # sessions updated within this window are "hot"
archive_after = "90d"           # move older sessions to cold storage
delete_after = "365d"           # permanently delete after this
checkpoint_retention = "forever" # always keep checkpoint summaries
```

Cold archival: compress event payloads, drop BrainThinking events (summaries already in checkpoints), retain Checkpoint + UserMessage + BrainResponse + ToolCall/Result + Error events.

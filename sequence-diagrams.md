# MOA Sequence Diagrams

Mermaid sequence diagrams showing how MOA actually moves at runtime. Start with §1 for the full end-to-end; the rest are zoom-ins on the same cast of participants.

**Participants** (consistent across every diagram):

| Short | Component | Crate |
|---|---|---|
| `User` | Person sending messages | — |
| `Platform` | Telegram / Slack / Discord / Desktop / CLI | `moa-gateway` (and `moa-desktop`, `moa-cli`) |
| `Gateway` | Normalizes inbound, renders outbound | `moa-gateway` |
| `Orch` | `BrainOrchestrator` (`LocalOrchestrator` or Restate-backed runtime) | `moa-orchestrator` |
| `Brain` | Stateless harness loop | `moa-brain` |
| `Pipe` | 7-stage context compilation pipeline | `moa-brain` |
| `LLM` | LLM provider (Anthropic / OpenAI / Gemini) | `moa-providers` |
| `Router` | `ToolRouter` — built-in / hand / MCP dispatch | `moa-hands` |
| `Hand` | `HandProvider` implementation | `moa-hands` |
| `Log` | `SessionStore` (Postgres) | `moa-session` |
| `Memory` | graph memory store, ingestion, and hybrid retrieval | `moa-memory-graph`, `moa-memory-ingest`, `moa-brain` |
| `Vault` | `CredentialVault` (file or HashiCorp) | `moa-security` |
| `Cron` | Scheduled-task runner (tokio-cron or Restate workflow delay) | `moa-orchestrator` |

---

## 1. Full end-to-end: "deploy to staging" via Telegram

The canonical request flow. Every other section is a zoom-in on a slice of this.

```mermaid
sequenceDiagram
    autonumber
    actor User
    participant Platform as Telegram
    participant Gateway
    participant Orch as Orchestrator
    participant Brain
    participant Pipe as Pipeline
    participant LLM
    participant Log as SessionStore
    participant Memory
    participant Router as ToolRouter
    participant Hand

    User->>Platform: "deploy to staging"
    Platform->>Gateway: InboundMessage {user, workspace, text}
    Gateway->>Orch: start_session / signal(QueueMessage)
    Orch->>Log: create_session + emit UserMessage
    Orch->>Brain: spawn / wake

    Brain->>Log: get_events(session_id)
    Log-->>Brain: EventRecord[]

    Brain->>Pipe: run 7 stages
    Pipe->>Memory: hybrid retrieve scoped graph nodes
    Memory-->>Pipe: ranked node hits + snippets
    Pipe->>Log: (reads history via Brain)
    Pipe-->>Brain: WorkingContext (cache_breakpoint marked)

    Brain->>LLM: complete(compiled_context)
    LLM-->>Brain: ToolCall { bash: "fly deploy --app staging" }

    Brain->>Log: emit ApprovalRequested
    Orch->>Gateway: observe event
    Gateway->>Platform: render inline buttons [Allow Once][Always][Deny]
    Platform-->>User: approval card

    User->>Platform: taps [Allow Once]
    Platform->>Gateway: callback_data
    Gateway->>Orch: signal(ApprovalDecided)
    Orch->>Brain: deliver signal
    Brain->>Log: emit ApprovalDecided

    Brain->>Router: execute("bash", "fly deploy ...")
    Router->>Hand: provision (lazy, first call)
    Hand-->>Router: HandHandle
    Router->>Hand: execute(bash, input)
    Hand-->>Router: ToolOutput { stdout, exit_code }
    Router-->>Brain: ToolOutput

    Brain->>Log: emit ToolCall + ToolResult

    Brain->>LLM: complete(updated_context)
    LLM-->>Brain: "Deployment complete. Staging is v2.3.1."
    Brain->>Log: emit BrainResponse

    opt ≥5 tool calls this session
        Brain->>Memory: fast remember learned deployment lesson
        Brain->>Log: emit memory learning event
    end

    Brain->>Log: emit SessionCompleted
    Orch->>Gateway: observe final event
    Gateway->>Platform: render final message
    Platform-->>User: "Deployment complete..."

    Note over Orch,Hand: On terminal exit, Orchestrator calls<br/>ToolRouter.destroy_session_hands(session_id)
```

---

## 2. Session start and brain wake

How a new message becomes a running brain. Shows the split between `start_session` (brand new) and `signal(QueueMessage)` (already running).

```mermaid
sequenceDiagram
    autonumber
    participant Platform
    participant Gateway
    participant Orch as Orchestrator
    participant Brain
    participant Log as SessionStore

    Platform->>Gateway: InboundMessage
    Gateway->>Orch: route by session mapping

    alt No active session for this thread
        Orch->>Log: create_session(meta)
        Log-->>Orch: session_id
        Orch->>Orch: spawn LocalBrainHandle<br/>(mpsc + broadcast + cancel tokens)
        Orch->>Brain: run_session_task(session_id, ...)
        Brain->>Log: update_status(Running)
    else Session already running
        Orch->>Brain: signal(QueueMessage)
        Brain->>Log: emit QueuedMessage
        Note over Brain: Processed after current turn
    end

    Brain->>Log: wake → get_events(from last Checkpoint)
    Log-->>Brain: EventRecord[]
    Note over Brain: Brain holds no pre-wake state —<br/>the log is the recovery mechanism
```

---

## 3. Context compilation pipeline (7 stages)

The stable-prefix layout that maximizes KV-cache reuse. Cost/latency depends heavily on this.

```mermaid
sequenceDiagram
    autonumber
    participant Brain
    participant Pipe as Pipeline
    participant Tools as ToolRegistry
    participant Skills as SkillRegistry
    participant Memory
    participant Log as SessionStore
    participant LLM

    Brain->>Pipe: run(WorkingContext)

    rect rgb(230, 245, 255)
        Note over Pipe: STABLE PREFIX (cached across turns)
        Pipe->>Pipe: 1. IdentityProcessor — static system prompt
        Pipe->>Pipe: 2. InstructionProcessor — workspace + user prefs
        Pipe->>Tools: 3. ToolDefinitionProcessor — get loadout (cap 30)
        Tools-->>Pipe: tool schemas (deterministic key order)
        Pipe->>Skills: 4. SkillInjector — metadata only (~100 tok/skill)
        Skills-->>Pipe: skill index
        Pipe->>Pipe: mark cache_breakpoint
    end

    rect rgb(255, 245, 230)
        Note over Pipe: DYNAMIC (changes per turn)
        Pipe->>Memory: 5. HybridRetriever — retrieve(query, ≤20% budget)
        Memory-->>Pipe: top-ranked graph nodes (truncated)
        Pipe->>Log: 6. HistoryCompiler — get_events(all)
        Log-->>Pipe: events
        Note over Pipe: checkpoint + last-5 verbatim<br/>older turns reverse-chronological<br/>errors ALWAYS preserved
        Pipe->>Pipe: 7. CacheOptimizer — verify prefix stability<br/>report cache_ratio
    end

    Pipe-->>Brain: WorkingContext
    Brain->>LLM: complete(request)
    LLM-->>Brain: CompletionStream

    Note over Pipe,LLM: Every stage logs ProcessorOutput:<br/>tokens_added, items_included, duration
```

---

## 4. Three-tier approval flow

What happens between the LLM producing a tool call and the hand executing. Approval can block for seconds or days — the brain is free to resume later from the log.

```mermaid
sequenceDiagram
    autonumber
    participant Brain
    participant Log as SessionStore
    participant Policy as ToolPolicies
    participant Orch as Orchestrator
    participant Gateway
    participant Platform
    actor User

    Brain->>Policy: check(tool, input, session_ctx)

    alt Allow rule matches (parsed-command level)
        Policy-->>Brain: Allow
        Note over Brain: Proceeds to execute (see §5)
    else Deny rule matches
        Policy-->>Brain: Deny
        Brain->>Log: emit ToolError(denied)
    else Requires approval
        Policy-->>Brain: RequireApproval
        Brain->>Log: emit ApprovalRequested {risk: low|med|high}
        Brain->>Log: update_status(WaitingApproval)

        Orch->>Gateway: observe ApprovalRequested
        Gateway->>Platform: render [Allow Once][Always Allow][Deny]
        Platform-->>User: risk-colored card (🟢/🟡/🔴)

        Note over Brain,User: Brain blocks on signal_rx.recv()<br/>Can wait indefinitely — durable

        User->>Platform: tap button
        Platform->>Gateway: callback
        Gateway->>Orch: signal(ApprovalDecided {decision, pattern?})
        Orch->>Brain: deliver signal

        alt Always Allow
            Brain->>Policy: persist rule (parsed pattern, not raw string)
            Note over Policy: "bash: npm test" does NOT approve<br/>"npm test && rm -rf /"
        end

        Brain->>Log: emit ApprovalDecided
        Brain->>Log: update_status(Running)
    end
```

---

## 5. Tool execution — lazy hand provisioning

How a tool call reaches the right execution environment. Hands are cattle: provisioned on first call, destroyed when the session ends.

```mermaid
sequenceDiagram
    autonumber
    participant Brain
    participant Router as ToolRouter
    participant BuiltIn
    participant Hand as HandProvider
    participant MCP as MCPClient
    participant Proxy as CredentialProxy
    participant Vault
    participant Log as SessionStore

    Brain->>Router: execute(tool, input, session_ctx)

    alt Built-in (memory_*, web_search, web_fetch)
        Router->>BuiltIn: execute(input, ctx)
        BuiltIn-->>Router: ToolOutput
    else Hand tool (bash, file_*, file_search)
        Router->>Router: get_or_provision_hand(provider, tier)
        alt Hand already cached for session
            Note over Router: Reuse HandHandle
        else First call this session
            Router->>Hand: provision(HandSpec)
            Hand-->>Router: HandHandle
        end
        Router->>Hand: execute(handle, tool, input)
        Hand-->>Router: ToolOutput {stdout, stderr, exit_code, duration}
    else MCP tool
        Router->>Proxy: session-scoped opaque token
        Proxy->>Vault: get(service, session_id)
        Vault-->>Proxy: real Credential
        Proxy->>MCP: tools/call with injected creds
        MCP-->>Proxy: result
        Note over Proxy: Brain NEVER sees real credentials
        Proxy-->>Router: ToolOutput (creds stripped)
    end

    Router-->>Brain: ToolOutput
    Brain->>Log: emit ToolCall + ToolResult

    Note over Router,Hand: On session terminal exit, Orchestrator calls<br/>Router.destroy_session_hands(session_id)
```

---

## 6. Memory: retrieve, write, and the learning loop

Memory is graph-native. Reads use hybrid retrieval over graph, sidecar, and vector indexes. Writes use the slow-path ingestion VO for documents and the fast-path memory API for short observations and lessons.

```mermaid
sequenceDiagram
    autonumber
    participant Brain
    participant LLM
    participant Retriever as HybridRetriever
    participant Ingest as IngestionVO
    participant Graph as GraphStore
    participant Vector as VectorStore
    participant Log as SessionStore

    Note over Brain,Retriever: Retrieval during context compilation
    Brain->>Retriever: retrieve(query, scope, limit)
    Retriever->>Graph: lookup seeds + expand neighbors
    Retriever->>Vector: vector search scoped embeddings
    Retriever-->>Brain: ranked graph nodes + snippets

    Note over Brain,Graph: Write triggers
    alt User correction
        Brain->>Graph: supersede fact or decision node
    else Discovery worth filing
        Brain->>Graph: fast_remember(observation)
    else Post-run skill distillation (≥5 tool calls)
        Brain->>Retriever: retrieve("task summary", scope, limit)
        Retriever-->>Brain: similar lessons/skills?
        alt Similar skill exists (similarity > 0.8)
            Brain->>LLM: "is the existing skill still best?"
            alt Unchanged
                Brain->>Graph: record usage metadata
            else Improved
                Brain->>Graph: create superseding skill/lesson node
            end
        else No similar skill
            Brain->>LLM: distill session → SKILL.md
            LLM-->>Brain: SKILL.md content
            Brain->>Ingest: ingest_turn(synthetic lesson turn)
        end
    end

    Ingest->>Graph: create nodes + edges
    Ingest->>Vector: write embeddings
    Brain->>Log: emit MemoryWrite
```

---

## 7. Compaction — the "memory flush + checkpoint" dance

Triggered when events-since-checkpoint > 100 **or** history tokens > 70% of context. Errors always survive.

```mermaid
sequenceDiagram
    autonumber
    participant Brain
    participant Log as SessionStore
    participant LLM
    participant Memory

    Brain->>Log: count events since last checkpoint
    Log-->>Brain: events_since_checkpoint

    alt < 100 AND tokens < 70%
        Note over Brain: No compaction needed
    else Compaction triggered
        Note over Brain,LLM: Phase 1 — Memory flush<br/>Give agent a chance to save facts
        Brain->>LLM: complete(flush_prompt, tools=[memory_remember])
        LLM-->>Brain: memory_remember tool calls
        Brain->>Memory: fast_remember × N
        Brain->>Log: emit MemoryWrite × N

        Note over Brain,LLM: Phase 2 — Summarize
        Brain->>LLM: complete(summary_prompt over flush_events)
        LLM-->>Brain: summary text (preserves errors, decisions, open items)

        Brain->>Log: emit Checkpoint {summary, events_summarized, token_count}
    end

    Note over Brain,Log: Next pipeline run — HistoryCompiler sees:<br/>[Checkpoint.summary] + [last 5 turns verbatim]<br/>+ ALL error events (never pruned)
```

---

## 8. Crash recovery — brains are disposable

Brains hold no pre-wake state. Kill the process, restart, pick up where the log left off.

```mermaid
sequenceDiagram
    autonumber
    participant OldBrain as Brain (old process)
    participant Log as SessionStore
    participant Orch as Orchestrator
    participant NewBrain as Brain (new process)

    OldBrain->>Log: emit ToolCall
    OldBrain->>Log: emit ToolResult
    Note over OldBrain: 💥 process crash / machine restart<br/>(panic, OOM, Fly.io suspend, Ctrl+C)

    Note over Orch: On next signal OR startup scan
    Orch->>Log: list_sessions(status in [Running, WaitingApproval])
    Log-->>Orch: recoverable sessions

    Orch->>NewBrain: spawn + wake(session_id)
    NewBrain->>Log: find last Checkpoint
    Log-->>NewBrain: Checkpoint @ seq=N (or none)
    NewBrain->>Log: get_events(from_seq = N+1)
    Log-->>NewBrain: events since checkpoint

    Note over NewBrain: Reconstructs context from log alone.<br/>No filesystem state required.<br/>Durable retries stay safe<br/>via UNIQUE(session_id, sequence_num).

    NewBrain->>NewBrain: run turn
```

---

## 9. Local mode wiring (`moa exec` / `moa-desktop`)

What gets wired up when you run without `MOA__CLOUD__ENABLED=true`.

```mermaid
sequenceDiagram
    autonumber
    actor User
    participant CLI as moa-cli / moa-desktop
    participant Orch as LocalOrchestrator
    participant Brain
    participant Log as PostgresSessionStore
    participant Hand as LocalHandProvider
    participant Vault as FileVault (age)
    participant Cron as tokio-cron-scheduler

    User->>CLI: moa exec "hello" (or launches desktop)
    CLI->>CLI: load ~/.moa/config.toml
    CLI->>CLI: detect Docker availability
    CLI->>Log: connect postgres://moa_owner:dev@localhost:5432/moa
    CLI->>Orch: new(store, memory, llm, router, vault)
    CLI->>Cron: start (consolidation, skill improvement)

    CLI->>Orch: start_session(prompt)
    Orch->>Brain: spawn tokio task
    Brain->>Log: emit UserMessage

    loop Brain loop
        Brain->>Log: get_events
        Brain->>Brain: pipeline → LLM → route tools
        alt Docker available
            Brain->>Hand: execute in local container
        else No Docker
            Brain->>Hand: direct exec (allowlisted)
        end
    end

    Brain-->>Orch: SessionCompleted
    Orch->>CLI: broadcast events
    CLI-->>User: stream to stderr, final to stdout
```

---

## 10. Cloud mode — Restate + Kubernetes

In cloud mode, each session is a Restate Virtual Object. Turns execute as durable handler invocations, approvals pause on awakeables, and the orchestrator scales as a standard Kubernetes deployment.

```mermaid
sequenceDiagram
    autonumber
    participant Platform
    participant Gateway
    participant Restate
    participant Session as Session VO
    participant LLM as LLMGateway
    participant Log as Postgres (Neon)
    participant Hand as Daytona / E2B
    participant K8s as Orchestrator pod

    Platform->>Gateway: inbound message
    Gateway->>Restate: invoke Session/post_message
    Restate->>K8s: route invocation
    K8s->>Session: post_message(session_id)

    loop Until SessionCompleted
        Session->>Log: get_events + compile context
        Session->>LLM: complete(request)
        alt ToolCall needs approval
            Session->>Log: emit ApprovalRequested
            Session->>Restate: await awakeable
            Note over Session,Restate: Invocation sleeps durably until resolved.
            Platform->>Gateway: user tapped button
            Gateway->>Restate: resolve awakeable
            Restate->>Session: resume
        else Tool executes
            Session->>Hand: provision (lazy) + execute
            Hand-->>Session: ToolOutput
            Session->>Log: emit ToolCall + ToolResult
        end
    end

    Session->>Log: emit SessionCompleted
    Note over K8s: Rolling restarts are safe.<br/>Restate replays durable state on retry.

    Note over Session,Log: emit_event is idempotent:<br/>UNIQUE(session_id, sequence_num)<br/>→ replay and retry remain safe
```

---

## 11. Consolidation ("Dream") — scheduled memory maintenance

Fires when ≥3 sessions complete AND ≥24h since last run. Runs as a delayed Restate workflow (cloud) or `tokio-cron-scheduler` job (local).

```mermaid
sequenceDiagram
    autonumber
    participant Cron
    participant Memory
    participant LLM
    participant Log as SessionStore

    Cron->>Memory: should_run_graph_maintenance?
    Memory-->>Cron: (recent writes, stale projections)

    alt >= 3 sessions AND >= 24h
        Cron->>Memory: query candidate nodes and contradictions
        Memory-->>Cron: graph maintenance candidates

        Cron->>LLM: run consolidation prompt<br/>(normalize dates, resolve contradictions,<br/>prune stale, dedupe, flag orphans)
        LLM-->>Cron: ConsolidationAction[]

        loop for each action
            alt SupersedeNode
                Cron->>Memory: supersede node
            else SoftDeleteNode
                Cron->>Memory: invalidate node
            else RefreshProjection
                Cron->>Memory: refresh sidecar projection
            else FlagOrphan
                Note over Cron: add to report (no side effect)
            end
        end

        Cron->>Log: emit maintenance report
    else
        Note over Cron: Skip this tick
    end
```

---

## 12. Concurrent memory writes (cloud) — graph supersession

In cloud mode, multiple brains may write memory concurrently. Graph writes are scoped transactions: conflicting facts are represented with supersession edges and indexed sidecar rows, rather than branch files.

```mermaid
sequenceDiagram
    autonumber
    participant BrainA as Brain A
    participant BrainB as Brain B
    participant Memory
    participant Reconciler
    participant LLM

    par Parallel writes to related facts
        BrainA->>Memory: fast_remember(fact A)
        Memory->>Memory: create node + embedding
    and
        BrainB->>Memory: fast_remember(fact B)
        Memory->>Memory: create node + embedding
    end

    Note over Reconciler: Inline contradiction judge or scheduled maintenance
    Reconciler->>Memory: query candidate conflicting nodes
    Memory-->>Reconciler: node pairs + evidence

    loop for each conflict
        Reconciler->>LLM: classify duplicate, supersede, or contradiction
        LLM-->>Reconciler: write decision
        Reconciler->>Memory: add SUPERSEDES / CONTRADICTS edge
        Reconciler->>Memory: update sidecar validity
    end
```

---

## 13. Observation — history-first, tail-second

`BrainOrchestrator::observe()` replays durable history, then attaches a live broadcast tail. If the tail lags beyond its buffer, the stream errors so callers can reconnect from durable state — silent loss is a bug.

```mermaid
sequenceDiagram
    autonumber
    participant Client as Observer (Gateway / Desktop)
    participant Orch as Orchestrator
    participant Log as SessionStore
    participant Brain
    participant Bcast as broadcast channel

    Client->>Orch: observe(session_id, level)

    Orch->>Log: get_events(all)
    Log-->>Orch: EventRecord[]
    Orch-->>Client: replay durable history

    alt Session is active
        Orch->>Bcast: subscribe
        Bcast-->>Orch: Receiver
        Orch-->>Client: attach live tail

        loop While session running
            Brain->>Bcast: emit EventRecord
            Bcast-->>Client: live event

            alt Subscriber lagged beyond buffer
                Bcast-->>Client: Err(Lagged)
                Note over Client: Client reconnects from<br/>last durable sequence it has seen
            end
        end
    else Session terminal
        Note over Client: No live tail needed
    end
```

---

## 14. Credential proxy — the brain never sees real credentials

Every external MCP tool call passes through the proxy. The brain holds only opaque session tokens; real credentials stay in the vault.

```mermaid
sequenceDiagram
    autonumber
    participant Brain
    participant Proxy as CredentialProxy
    participant Vault
    participant MCP as MCPClient
    participant Ext as External service<br/>(GitHub / DB / etc.)

    Note over Proxy: On session start
    Brain->>Proxy: create_session_token(session_id, service)
    Proxy->>Proxy: store {token → session_id + service + expiry}
    Proxy-->>Brain: "moa_sess_<uuid>" (opaque)

    Note over Brain: Brain uses opaque token in tool calls
    Brain->>MCP: tools/call with session_token
    MCP->>Proxy: enrich_request(session_token, request)

    Proxy->>Proxy: lookup session_token
    Proxy->>Vault: get(service, session_id)
    Vault-->>Proxy: real Credential

    Proxy->>Proxy: inject creds into headers<br/>(Bearer / OAuth / ApiKey)
    Proxy->>Ext: authenticated request
    Ext-->>Proxy: response

    Proxy-->>MCP: response (creds stripped)
    MCP-->>Brain: ToolOutput
    Note over Brain: Brain's context never contains<br/>API keys, OAuth tokens, or passwords
```

---

## Related docs

- [`architecture.md`](architecture.md) — structural overview of all the components shown above
- [`docs/01-architecture-overview.md`](docs/01-architecture-overview.md) — full trait signatures
- [`docs/02-brain-orchestration.md`](docs/02-brain-orchestration.md) — Restate + local orchestrator internals
- [`docs/03-communication-layer.md`](docs/03-communication-layer.md) — approval UX, observation verbosity, rate limits
- [`docs/04-memory-architecture.md`](docs/04-memory-architecture.md) — graph memory, ingestion, retrieval, sidecar indexes
- [`docs/05-session-event-log.md`](docs/05-session-event-log.md) — event schema, compaction, replay
- [`docs/06-hands-and-mcp.md`](docs/06-hands-and-mcp.md) — HandProvider, ToolRouter, MCP
- [`docs/07-context-pipeline.md`](docs/07-context-pipeline.md) — 7-stage pipeline details
- [`docs/08-security.md`](docs/08-security.md) — sandbox tiers, credential isolation, injection defense

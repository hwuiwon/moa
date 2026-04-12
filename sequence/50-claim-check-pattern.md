# Step 50 — Claim-Check Pattern for Large Event Payloads

_Offload large event payloads (diffs, tool outputs, file contents) to blob storage. Store a reference in the event log. Resolve lazily on read._

---

## 1. What this step is about

MOA's session event log stores full payloads inline — tool outputs, file diffs, approval prompts, brain responses. A single `file_read` of a 50KB file produces a 50KB event row. A session with heavy file operations can produce a multi-megabyte event log, which slows down `get_events()`, inflates database size, and makes event log FTS indexing expensive.

The claim-check pattern (from enterprise integration patterns) solves this: store large payloads in a separate blob store, replace the payload in the event with a reference (the "claim check"), and resolve the reference lazily when the event is actually read.

There is already a `TODO` in `moa-core/src/events.rs` (line 124) for this.

---

## 2. Files/directories to read

- **`moa-core/src/events.rs`** — `Event` enum. The `ApprovalRequested` variant has the TODO. But all event variants with potentially large payloads need this: `ToolResult.output`, `BrainResponse.text`, `ToolCall.input`, `ApprovalRequested.prompt`.
- **`moa-core/src/types.rs`** — `ToolOutput`, `ApprovalPrompt`. Types containing large fields.
- **`moa-session/src/`** — `SessionStore` implementations. `emit_event()` serializes events to JSON and stores them. `get_events()` deserializes them. Both need claim-check awareness.
- **`moa-core/src/traits.rs`** — `SessionStore` trait. May need a `BlobStore` companion trait or the blob store can be internal to the `SessionStore` implementation.
- **`moa-core/src/config.rs`** — Need a config for blob storage location and size threshold.

---

## 3. Goal

After this step:

1. Event payloads larger than a configurable threshold (default: 64KB) are stored in a separate blob store.
2. The event row in the database contains a `ClaimCheck { blob_id, size, content_type }` reference instead of the full payload.
3. `get_events()` transparently resolves claim checks — callers see the full payload as if it were inline.
4. Blob storage is local filesystem in local mode (`~/.moa/blobs/`) and object storage in cloud mode (S3/R2 — future, out of scope for this step).
5. The session database stays small and fast regardless of how much file content the agent reads/writes.

---

## 4. Rules

- **Transparent to callers.** `SessionStore::get_events()` returns fully-resolved `Event` values. No caller needs to know about claim checks. Resolution happens inside the store implementation.
- **Lazy resolution is optional.** For the initial implementation, resolve eagerly in `get_events()`. Add lazy resolution (return a reference, resolve on field access) only if profiling shows it's needed.
- **Threshold is configurable.** Default 64KB. Set to 0 to disable (store everything inline). Set very low for testing.
- **Blob store is append-only.** Blobs are never modified. They can be garbage-collected when the session is archived/deleted.
- **Blob IDs are content-addressed.** Use SHA-256 hash of the payload. Identical payloads share the same blob. This deduplicates repeated `file_read` calls on the same file.
- **The claim check is a new wrapper type, not a change to the `Event` enum.** Individual event fields that can be large should use a `MaybeBlob<String>` type that's either `Inline(String)` or `BlobRef(ClaimCheck)`.
- **FTS indexing uses truncated content.** For events with blob-stored payloads, the FTS entry should contain a truncated preview (first 1KB), not the full payload. This keeps the FTS index fast.
- **Local blobs only in this step.** Cloud object storage (S3/R2) is a future concern. Local mode uses filesystem blobs at `~/.moa/blobs/{session_id}/{blob_id}`.

---

## 5. Tasks

### 5a. Define `ClaimCheck` and `MaybeBlob` types in `moa-core/src/types.rs`

```rust
/// Reference to a payload stored in the blob store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimCheck {
    /// Content-addressed blob identifier (SHA-256 hex).
    pub blob_id: String,
    /// Original payload size in bytes.
    pub size: usize,
    /// Preview of the content (first 1KB) for FTS indexing and quick inspection.
    pub preview: String,
}

/// A string value that may be stored inline or offloaded to blob storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MaybeBlob {
    /// Payload stored inline in the event.
    Inline(String),
    /// Payload offloaded to blob storage.
    BlobRef(ClaimCheck),
}

impl MaybeBlob {
    /// Returns the inline content, or panics if this is a blob ref.
    /// Use `resolve()` to ensure the value is inline first.
    pub fn as_str(&self) -> &str {
        match self {
            MaybeBlob::Inline(s) => s,
            MaybeBlob::BlobRef(check) => &check.preview,
        }
    }

    /// Returns true if the payload has been offloaded.
    pub fn is_blob_ref(&self) -> bool {
        matches!(self, MaybeBlob::BlobRef(_))
    }
    
    /// Returns the full content, whether inline or blob-stored.
    /// For blob refs, this requires the content to have been resolved.
    pub fn into_string(self) -> String {
        match self {
            MaybeBlob::Inline(s) => s,
            MaybeBlob::BlobRef(check) => check.preview, // fallback to preview if not resolved
        }
    }
}
```

### 5b. Define `BlobStore` trait in `moa-core/src/traits.rs`

```rust
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Store a blob and return its content-addressed ID.
    async fn store(&self, session_id: &SessionId, content: &[u8]) -> Result<String>;
    
    /// Retrieve a blob by ID.
    async fn get(&self, session_id: &SessionId, blob_id: &str) -> Result<Vec<u8>>;
    
    /// Delete all blobs for a session (cleanup).
    async fn delete_session(&self, session_id: &SessionId) -> Result<()>;
    
    /// Check if a blob exists.
    async fn exists(&self, session_id: &SessionId, blob_id: &str) -> Result<bool>;
}
```

### 5c. Implement `FileBlobStore` in `moa-session/src/`

```rust
pub struct FileBlobStore {
    base_dir: PathBuf,  // ~/.moa/blobs/
}

impl FileBlobStore {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }
    
    fn blob_path(&self, session_id: &SessionId, blob_id: &str) -> PathBuf {
        self.base_dir.join(session_id.to_string()).join(blob_id)
    }
}

#[async_trait]
impl BlobStore for FileBlobStore {
    async fn store(&self, session_id: &SessionId, content: &[u8]) -> Result<String> {
        use sha2::{Sha256, Digest};
        let blob_id = hex::encode(Sha256::digest(content));
        
        let path = self.blob_path(session_id, &blob_id);
        if !path.exists() {
            fs::create_dir_all(path.parent().unwrap()).await?;
            fs::write(&path, content).await?;
        }
        
        Ok(blob_id)
    }
    
    async fn get(&self, session_id: &SessionId, blob_id: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(session_id, blob_id);
        fs::read(&path).await.map_err(|e| MoaError::BlobNotFound(blob_id.to_string()))
    }
    
    async fn delete_session(&self, session_id: &SessionId) -> Result<()> {
        let dir = self.base_dir.join(session_id.to_string());
        if dir.exists() {
            fs::remove_dir_all(&dir).await.ok();
        }
        Ok(())
    }
}
```

### 5d. Modify `SessionStore::emit_event()` to offload large payloads

In the session store implementation (both Turso and Postgres), before serializing the event:

```rust
async fn emit_event(&self, session_id: SessionId, mut event: Event) -> Result<SequenceNum> {
    // Offload large payloads to blob store
    self.offload_large_payloads(&session_id, &mut event).await?;
    
    // Serialize and store (now with claim checks instead of large payloads)
    let payload = serde_json::to_string(&event)?;
    // ... existing insert logic ...
}

async fn offload_large_payloads(&self, session_id: &SessionId, event: &mut Event) -> Result<()> {
    let threshold = self.config.blob_threshold_bytes;
    if threshold == 0 { return Ok(()); }  // disabled
    
    match event {
        Event::ToolResult { output, .. } => {
            offload_field(&self.blob_store, session_id, output, threshold).await?;
        }
        Event::BrainResponse { text, .. } => {
            offload_field(&self.blob_store, session_id, text, threshold).await?;
        }
        Event::ToolCall { input, .. } => {
            // input is serde_json::Value — serialize to check size
            let input_str = input.to_string();
            if input_str.len() > threshold {
                // Store as blob, replace with claim check in a wrapper
            }
        }
        // ... other large-payload events ...
        _ => {}
    }
    Ok(())
}
```

### 5e. Modify `SessionStore::get_events()` to resolve claim checks

```rust
async fn get_events(&self, session_id: SessionId, range: EventRange) -> Result<Vec<EventRecord>> {
    let records = self.get_events_raw(session_id, range).await?;
    
    // Resolve any claim checks
    let mut resolved = Vec::with_capacity(records.len());
    for mut record in records {
        self.resolve_claim_checks(&session_id, &mut record.event).await?;
        resolved.push(record);
    }
    
    Ok(resolved)
}
```

### 5f. Update FTS indexing for blob-stored events

When inserting into the FTS index (`events_fts`), use the `preview` field from the claim check instead of the full payload:

```rust
let fts_content = match &event {
    Event::ToolResult { output, .. } => match output {
        MaybeBlob::Inline(s) => s.clone(),
        MaybeBlob::BlobRef(check) => check.preview.clone(),
    },
    // ... similar for other events ...
    _ => serde_json::to_string(&event)?,
};
```

### 5g. Add blob config

```toml
[session]
blob_threshold_bytes = 65536   # 64KB. Set to 0 to disable.
blob_dir = "~/.moa/blobs"      # Local blob storage path
```

### 5h. Wire blob store into SessionStore construction

In `moa-session/src/`, when creating the session store, also create the blob store and pass it in.

---

## 6. How it should be implemented

The most important design decision: **where does `MaybeBlob` appear in the type system?**

**Option A: Change `Event` field types.** Replace `String` fields with `MaybeBlob` in `Event::ToolResult`, `Event::BrainResponse`, etc. This is the most type-safe approach but requires touching every place that reads these fields.

**Option B: Handle in the serialization layer.** Keep `Event` fields as `String`, but in `emit_event()` replace the string with a JSON-encoded claim check marker, and in `get_events()` detect and resolve the marker. Less invasive but less type-safe.

**Recommendation: Option B for this step.** It minimizes changes to the `Event` enum (which is used across many crates) while getting the storage benefit. Use a recognizable marker format in the JSON:

```json
{"__moa_blob_ref": {"blob_id": "abc123...", "size": 52000, "preview": "first 1KB..."}}
```

In `emit_event()`, scan the serialized JSON for large string values and replace them. In `get_events()`, scan for markers and resolve them. This is purely a storage-layer concern — no crate outside `moa-session` needs to change.

---

## 7. Deliverables

- [ ] `moa-core/src/traits.rs` — `BlobStore` trait
- [ ] `moa-core/src/config.rs` — `blob_threshold_bytes` and `blob_dir` config fields
- [ ] `moa-session/src/blob.rs` — `FileBlobStore` implementation
- [ ] `moa-session/src/` — Modified `emit_event()` to offload large payloads, modified `get_events()` to resolve claim checks
- [ ] `moa-session/Cargo.toml` — Add `sha2` and `hex` dependencies
- [ ] `docs/sample-config.toml` — `[session]` blob config

---

## 8. Acceptance criteria

1. **Large payloads offloaded.** A `file_read` of a 100KB file produces an event row < 2KB in the database, with the full content in `~/.moa/blobs/`.
2. **Small payloads inline.** A `file_read` of a 1KB file stays inline — no blob created.
3. **Transparent resolution.** `get_events()` returns the full payload for blob-stored events. Callers see no difference.
4. **Content-addressed deduplication.** Reading the same 100KB file twice creates one blob, not two.
5. **FTS works with previews.** Searching events by keyword finds blob-stored events via the preview text.
6. **Session cleanup.** Deleting a session removes its blobs.
7. **Config respected.** `blob_threshold_bytes = 0` disables offloading entirely.
8. **No regressions.** All existing session store tests pass.

---

## 9. Testing

**Test 1:** `large_payload_offloaded` — Emit a ToolResult with 100KB output, verify the event row in DB is < 2KB, verify blob file exists.

**Test 2:** `small_payload_stays_inline` — Emit a ToolResult with 500 byte output, verify no blob file created.

**Test 3:** `get_events_resolves_claim_checks` — Emit large event, read it back, verify full content returned.

**Test 4:** `content_addressed_dedup` — Emit two events with identical 100KB payloads, verify only one blob file.

**Test 5:** `fts_searches_preview` — Emit a large event whose first 1KB contains "deploy error", search for "deploy error", verify found.

**Test 6:** `session_delete_removes_blobs` — Emit blobs, delete session, verify blob directory removed.

**Test 7:** `threshold_zero_disables` — Set threshold to 0, emit 1MB event, verify stored inline (no blob).

**Test 8:** `blob_store_idempotent` — Store the same content twice, verify same blob_id returned.

---

## 10. Additional notes

- **Why content-addressed?** SHA-256 hashing ensures deduplication without a lookup table. If the agent reads the same file 10 times in a session (common during debugging), only one blob is stored.
- **Preview size.** 1KB preview captures enough for FTS search and quick inspection in the TUI. The TUI can show the preview with a "[+52KB more]" indicator and fetch the full content on expand.
- **Future: cloud blob storage.** The `BlobStore` trait is designed for pluggable backends. A future `S3BlobStore` or `R2BlobStore` can be added behind a feature flag without changing the session store code.
- **Future: lazy resolution.** For `get_events()` calls that only need metadata (event types, timestamps), skipping blob resolution would be faster. Add a `get_events_metadata()` variant or a flag on `EventRange` to control resolution.
- **Garbage collection.** Blobs are never modified, only created and eventually deleted with the session. No GC needed beyond session lifecycle management.

//! Durable fjall journal for pending lineage rows.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use fjall::{
    KeyspaceCreateOptions, PersistMode, Readable, SingleWriterTxDatabase, SingleWriterTxKeyspace,
};

use crate::{Error, Result};

/// Durable journal storing rows that have not yet reached TimescaleDB.
pub struct Journal {
    keyspace: SingleWriterTxDatabase,
    partition: SingleWriterTxKeyspace,
    persists: AtomicU64,
}

impl Journal {
    /// Opens or creates a journal at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let keyspace = SingleWriterTxDatabase::builder(path).open()?;
        let partition = keyspace.keyspace("lineage-pending", KeyspaceCreateOptions::default)?;
        Ok(Self {
            keyspace,
            partition,
            persists: AtomicU64::new(0),
        })
    }

    /// Appends one pending row under the provided sequence number with its own durability sync.
    ///
    /// This single-row form fsyncs per call; it backs the awaitable durability path where each
    /// event must be synced before its caller is acknowledged. High-throughput callers should
    /// prefer [`Journal::append_batch`] so a batch costs one sync window instead of `N`.
    pub fn append(&self, seq: u64, payload: &[u8]) -> Result<()> {
        self.partition.insert(seq.to_be_bytes(), payload)?;
        self.persist()?;
        Ok(())
    }

    /// Appends a batch of pending rows under a single durability sync (group commit).
    ///
    /// Rows are inserted in the caller-provided order and one `SyncData` persist covers the whole
    /// batch, so `N` appends cost one sync window rather than `N`.
    pub fn append_batch(&self, entries: &[(u64, Vec<u8>)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        for (seq, payload) in entries {
            self.partition.insert(seq.to_be_bytes(), payload)?;
        }
        self.persist()?;
        Ok(())
    }

    /// Acknowledges a sequence range after a successful database write.
    pub fn ack_range(&self, lo: u64, hi: u64) -> Result<()> {
        if lo > hi {
            return Ok(());
        }

        let mut tx = self.keyspace.write_tx();
        for seq in lo..=hi {
            tx.remove(&self.partition, seq.to_be_bytes());
        }
        tx.commit()?;
        self.persist()?;
        Ok(())
    }

    /// Replays pending rows whose sequence number is greater than `after_seq`, in sequence order.
    ///
    /// Callers keep a low-water-mark cursor and pass the highest already-processed sequence so the
    /// scan only visits newly journaled rows instead of the entire pending set on every call. The
    /// underlying range scan skips lower keys at the index level, so retained (dead-lettered) rows
    /// below the cursor are not re-read.
    pub fn replay_from(&self, after_seq: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        let mut out = Vec::new();
        let read_tx = self.keyspace.read_tx();
        let start = after_seq.saturating_add(1).to_be_bytes();
        for kv in read_tx.range(&self.partition, start..) {
            let (key, value) = kv.into_inner()?;
            let bytes: [u8; 8] = key
                .as_ref()
                .try_into()
                .map_err(|_| Error::InvalidJournalKey)?;
            out.push((u64::from_be_bytes(bytes), value.to_vec()));
        }
        out.sort_by_key(|(seq, _)| *seq);
        Ok(out)
    }

    /// Replays all pending rows in sequence order.
    pub fn replay(&self) -> Result<Vec<(u64, Vec<u8>)>> {
        self.replay_from(0)
    }

    /// Returns the approximate pending row count.
    #[must_use]
    pub fn approximate_len(&self) -> usize {
        self.partition.approximate_len()
    }

    /// Returns the lowest-sequence pending entry, or `None` when the journal is empty.
    ///
    /// Used to report the oldest unacknowledged row's age. The scan starts at
    /// sequence 0 and takes the first key, so it is bounded by the index seek
    /// rather than the full pending set.
    pub fn oldest_entry(&self) -> Result<Option<(u64, Vec<u8>)>> {
        let read_tx = self.keyspace.read_tx();
        let start = 0_u64.to_be_bytes();
        let Some(kv) = read_tx.range(&self.partition, start..).next() else {
            return Ok(None);
        };
        let (key, value) = kv.into_inner()?;
        let bytes: [u8; 8] = key
            .as_ref()
            .try_into()
            .map_err(|_| Error::InvalidJournalKey)?;
        Ok(Some((u64::from_be_bytes(bytes), value.to_vec())))
    }

    /// Returns the number of durability syncs issued so far.
    ///
    /// Exposed so group-commit tests can assert that a batch append performs exactly one sync.
    #[cfg(test)]
    #[must_use]
    pub fn persist_count(&self) -> u64 {
        self.persists.load(Ordering::Relaxed)
    }

    fn persist(&self) -> Result<()> {
        self.keyspace.persist(PersistMode::SyncData)?;
        self.persists.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

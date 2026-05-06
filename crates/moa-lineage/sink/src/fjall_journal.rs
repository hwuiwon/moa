//! Durable fjall journal for pending lineage rows.

use std::path::Path;

use fjall::{
    KeyspaceCreateOptions, PersistMode, Readable, SingleWriterTxDatabase, SingleWriterTxKeyspace,
};

use crate::{Error, Result};

/// Durable journal storing rows that have not yet reached TimescaleDB.
pub struct Journal {
    keyspace: SingleWriterTxDatabase,
    partition: SingleWriterTxKeyspace,
}

impl Journal {
    /// Opens or creates a journal at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let keyspace = SingleWriterTxDatabase::builder(path).open()?;
        let partition = keyspace.keyspace("lineage-pending", KeyspaceCreateOptions::default)?;
        Ok(Self {
            keyspace,
            partition,
        })
    }

    /// Appends one pending row under the provided sequence number.
    pub fn append(&self, seq: u64, payload: &[u8]) -> Result<()> {
        self.partition.insert(seq.to_be_bytes(), payload)?;
        self.keyspace.persist(PersistMode::SyncData)?;
        Ok(())
    }

    /// Acknowledges a sequence range after successful database write.
    pub fn ack_range(&self, lo: u64, hi: u64) -> Result<()> {
        if lo > hi {
            return Ok(());
        }

        let mut tx = self.keyspace.write_tx();
        for seq in lo..=hi {
            tx.remove(&self.partition, seq.to_be_bytes());
        }
        tx.commit()?;
        self.keyspace.persist(PersistMode::SyncData)?;
        Ok(())
    }

    /// Replays pending rows in sequence order.
    pub fn replay(&self) -> Result<Vec<(u64, Vec<u8>)>> {
        let mut out = Vec::new();
        let read_tx = self.keyspace.read_tx();
        for kv in read_tx.iter(&self.partition) {
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

    /// Returns the approximate pending row count.
    #[must_use]
    pub fn approximate_len(&self) -> usize {
        self.partition.approximate_len()
    }
}

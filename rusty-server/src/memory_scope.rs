//! A run's memory: the tenant's store narrowed to the scopes a run may
//! read — the person it is for and the agent it is of. The context
//! pipeline reads memory through the run's journaled seam and never sees
//! a record outside those scopes, so one person's memory cannot reach
//! another's run whatever the query says.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::error::Result;
use rusty_agent_runtime::memory::{MemoryQuery, MemoryRecord, MemoryStore, ScopeAddress};

/// The tenant's store, narrowed to `scopes`. Writes pass through — a run
/// writes through its tools, which scope their own records — reads are
/// filtered.
#[derive(Debug)]
pub struct ScopedMemoryStore {
    inner: Arc<dyn MemoryStore>,
    scopes: Vec<ScopeAddress>,
}

impl ScopedMemoryStore {
    pub fn new(inner: Arc<dyn MemoryStore>, scopes: Vec<ScopeAddress>) -> Self {
        Self { inner, scopes }
    }

    fn admits(&self, record: &MemoryRecord) -> bool {
        self.scopes.contains(&record.scope)
    }
}

#[async_trait]
impl MemoryStore for ScopedMemoryStore {
    async fn put(&self, record: &MemoryRecord) -> Result<bool> {
        self.inner.put(record).await
    }

    async fn get(&self, memory_id: &str) -> Result<Option<MemoryRecord>> {
        Ok(self.inner.get(memory_id).await?.filter(|record| self.admits(record)))
    }

    async fn all(&self) -> Result<Vec<MemoryRecord>> {
        let mut records = self.inner.all().await?;
        records.retain(|record| self.admits(record));
        Ok(records)
    }

    async fn remove(&self, memory_id: &str) -> Result<bool> {
        self.inner.remove(memory_id).await
    }

    fn utility_bps(&self, memory_id: &str) -> Option<u32> {
        self.inner.utility_bps(memory_id)
    }

    async fn query(&self, query: &MemoryQuery, now: DateTime<Utc>) -> Result<Vec<MemoryRecord>> {
        let mut records = self.inner.query(query, now).await?;
        records.retain(|record| self.admits(record));
        Ok(records)
    }
}

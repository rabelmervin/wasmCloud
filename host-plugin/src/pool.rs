//! Connection pool registry (sqlx / MySqlPool)
//!
//! Maintains one `sqlx::MySqlPool` per unique connection URL.
//! Pools are keyed by the connection URL so that multiple WASM components
//! sharing the same database share the same underlying connection pool.

use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared pool builder used by both `add` and `get_or_create`.
///
/// acquire_timeout: 30s — WSL2→Docker bridge can be slow; 3s caused pool
/// timeouts when the connection needed to be re-established after an idle gap.
/// idle_timeout: 5 min — prevents stale connections without churning the pool.
fn pool_options() -> MySqlPoolOptions {
    MySqlPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .idle_timeout(std::time::Duration::from_secs(300))
}

// ── error type ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("No connection pool found for source: {0}")]
    NotFound(String),
    #[error("Invalid database URL '{url}': {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error("SQL error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

// ── registry ──────────────────────────────────────────────────────────────────

/// Shared pool registry.  Cheap to clone (Arc inside).
#[derive(Clone, Default)]
pub struct PoolRegistry {
    inner: Arc<RwLock<HashMap<String, MySqlPool>>>,
}

impl PoolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the pool for a source-component link.
    pub async fn add(&self, source_id: &str, database_url: &str) -> Result<(), PoolError> {
        let pool = pool_options()
            .connect_lazy(database_url)
            .map_err(|e| PoolError::InvalidUrl {
                url: database_url.to_string(),
                reason: e.to_string(),
            })?;
        self.inner
            .write()
            .await
            .insert(source_id.to_string(), pool);
        tracing::info!(source_id, "registered MySQL pool");
        Ok(())
    }

    /// Remove the pool for a source component (called on link deletion).
    pub async fn remove(&self, source_id: &str) {
        if let Some(pool) = self.inner.write().await.remove(source_id) {
            pool.close().await;
        }
        tracing::info!(source_id, "removed MySQL pool");
    }

    /// Retrieve the pool for a given source component.
    pub async fn get(&self, source_id: &str) -> Result<MySqlPool, PoolError> {
        self.inner
            .read()
            .await
            .get(source_id)
            .cloned()
            .ok_or_else(|| PoolError::NotFound(source_id.to_string()))
    }

    /// Return (or lazily create) a pool for the given connection URL.
    /// Used by the wRPC handler and the HostPlugin — both pass the URL per-call.
    pub async fn get_or_create(&self, connection_url: &str) -> Result<MySqlPool, PoolError> {
        // Fast path: pool already exists
        {
            let guard = self.inner.read().await;
            if let Some(pool) = guard.get(connection_url) {
                return Ok(pool.clone());
            }
        }
        // Slow path: create a lazy pool and cache it
        let pool = pool_options()
            .connect_lazy(connection_url)
            .map_err(|e| PoolError::InvalidUrl {
                url: connection_url.to_string(),
                reason: e.to_string(),
            })?;
        self.inner
            .write()
            .await
            .insert(connection_url.to_string(), pool.clone());
        Ok(pool)
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_and_remove_pool() {
        let registry = PoolRegistry::new();
        registry
            .add("source-1", "mysql://user:pass@localhost:3306/testdb")
            .await
            .expect("valid URL should parse");
        assert!(registry.get("source-1").await.is_ok());
        registry.remove("source-1").await;
        assert!(registry.get("source-1").await.is_err());
    }

    #[tokio::test]
    async fn get_missing_source_returns_error() {
        let registry = PoolRegistry::new();
        let err = registry.get("nonexistent").await.unwrap_err();
        assert!(matches!(err, PoolError::NotFound(_)));
    }

    #[tokio::test]
    async fn invalid_url_returns_error() {
        let registry = PoolRegistry::new();
        let err = registry
            .add("bad-source", "not-a-valid-url")
            .await
            .unwrap_err();
        assert!(matches!(err, PoolError::InvalidUrl { .. }));
    }

    #[tokio::test]
    async fn multiple_sources_are_independent() {
        let registry = PoolRegistry::new();
        for i in 0..5 {
            registry
                .add(
                    &format!("source-{i}"),
                    &format!("mysql://user:pass@localhost:{}/db{i}", 3306 + i),
                )
                .await
                .expect("valid URL");
        }
        for i in 0..5 {
            assert!(registry.get(&format!("source-{i}")).await.is_ok());
        }
        registry.remove("source-2").await;
        assert!(registry.get("source-2").await.is_err());
        assert!(registry.get("source-0").await.is_ok());
        assert!(registry.get("source-4").await.is_ok());
    }
}

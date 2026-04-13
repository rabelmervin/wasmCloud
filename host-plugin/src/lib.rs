//! Graphily MySQL host plugin — wash-runtime HostPlugin for `graphily:mysql/mysql-api`.
//!
//! Compiled into the wasmCloud fork's `wash host` binary alongside the built-in plugins.
//! MysqlProvider implements HostPlugin and is registered via:
//!   ClusterHostBuilder::with_plugin(Arc::new(MysqlProvider::new()))

pub mod executor;
pub mod host_plugin;
pub mod pool;

use pool::PoolRegistry;

// ── Global provider ───────────────────────────────────────────────────────────
//
// MySQL host functions are registered on data-engine's linker via `add_to_linker`,
// but when data-engine runs inside another component's wasmtime store (e.g.,
// gateway's store in a multi-component workload), `self` in the host trait
// methods is *gateway's* Ctx — which has no MysqlProvider in its plugin map.
//
// Solution: store the provider here once at startup and access it directly,
// bypassing the per-Ctx plugin registry entirely.
pub(crate) static GLOBAL_PROVIDER: std::sync::OnceLock<MysqlProvider> =
    std::sync::OnceLock::new();

// ── MysqlProvider ─────────────────────────────────────────────────────────────

/// MySQL host plugin for `graphily:mysql/mysql-api`.
/// Wired into the wasmCloud host in-process via the Wasmtime linker.
#[derive(Clone, Default)]
pub struct MysqlProvider {
    /// Per-URL connection pools.
    pub pools: PoolRegistry,
}

impl MysqlProvider {
    pub const PLUGIN_ID: &'static str = "graphily-mysql";

    pub fn new() -> Self {
        Self::default()
    }
}

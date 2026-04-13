//! wash-runtime `HostPlugin` implementation for `graphily:mysql/mysql-api`.

use std::collections::HashSet;

use wash_runtime::{
    engine::ctx::{ActiveCtx, SharedCtx, extract_active_ctx},
    engine::workload::WorkloadItem,
    plugin::HostPlugin,
    wit::{WitInterface, WitWorld},
};

use crate::MysqlProvider;

// ── Wasmtime component bindings for the host side ────────────────────────────

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "mysql-host",
        imports: { default: async | trappable },
        wasmtime_crate: wash_runtime::wasmtime,
    });
}

// ── Host trait implementation ─────────────────────────────────────────────────
//
// Implemented on ActiveCtx<'a> (the per-call execution context), matching the
// pattern used by all wash-runtime built-in plugins (e.g. wasmcloud_postgres).
//
// Pool access goes through GLOBAL_PROVIDER to handle the case where data-engine
// runs inside gateway's wasmtime store — gateway's ActiveCtx has no MysqlProvider
// in its plugin map, but GLOBAL_PROVIDER is always available.

impl<'a> bindings::graphily::mysql::mysql_api::Host for ActiveCtx<'a> {
    async fn execute_query(
        &mut self,
        connection_url: String,
        sql: String,
        params: Vec<String>,
    ) -> wasmtime::Result<Result<bindings::graphily::mysql::mysql_api::SqlResult, String>> {
        tracing::info!(plugin = "graphily-mysql", url = %connection_url, sql = %sql, "execute_query called");
        let Some(plugin) = crate::GLOBAL_PROVIDER.get() else {
            return Ok(Err("MysqlProvider not initialized — host.start() not called".to_string()));
        };
        let pool = match plugin.pools.get_or_create(&connection_url).await {
            Ok(p) => p,
            Err(e) => return Ok(Err(e.to_string())),
        };
        let result = crate::executor::execute_query(&pool, &sql, params).await;
        tracing::info!(plugin = "graphily-mysql", rows = result.as_ref().map(|r| r.rows.len()).unwrap_or(0), "execute_query done");
        Ok(result
            .map(|r| bindings::graphily::mysql::mysql_api::SqlResult {
                columns: r.columns,
                rows: r.rows,
                rows_affected: r.rows_affected,
                last_insert_id: r.last_insert_id,
            }))
    }

    async fn execute_mutation(
        &mut self,
        connection_url: String,
        sql: String,
        params: Vec<String>,
    ) -> wasmtime::Result<Result<bindings::graphily::mysql::mysql_api::SqlResult, String>> {
        let Some(plugin) = crate::GLOBAL_PROVIDER.get() else {
            return Ok(Err("MysqlProvider not initialized — host.start() not called".to_string()));
        };
        let pool = match plugin.pools.get_or_create(&connection_url).await {
            Ok(p) => p,
            Err(e) => return Ok(Err(e.to_string())),
        };
        Ok(crate::executor::execute_mutation(&pool, &sql, params)
            .await
            .map(|r| bindings::graphily::mysql::mysql_api::SqlResult {
                columns: r.columns,
                rows: r.rows,
                rows_affected: r.rows_affected,
                last_insert_id: r.last_insert_id,
            }))
    }

    async fn execute_transaction(
        &mut self,
        connection_url: String,
        statements: Vec<(String, Vec<String>)>,
    ) -> wasmtime::Result<Result<Vec<bindings::graphily::mysql::mysql_api::SqlResult>, String>> {
        let Some(plugin) = crate::GLOBAL_PROVIDER.get() else {
            return Ok(Err("MysqlProvider not initialized — host.start() not called".to_string()));
        };
        let pool = match plugin.pools.get_or_create(&connection_url).await {
            Ok(p) => p,
            Err(e) => return Ok(Err(e.to_string())),
        };
        Ok(crate::executor::execute_transaction(&pool, statements)
            .await
            .map(|v| {
                v.into_iter()
                    .map(|r| bindings::graphily::mysql::mysql_api::SqlResult {
                        columns: r.columns,
                        rows: r.rows,
                        rows_affected: r.rows_affected,
                        last_insert_id: r.last_insert_id,
                    })
                    .collect()
            }))
    }

    async fn ping(
        &mut self,
        connection_url: String,
    ) -> wasmtime::Result<Result<(), String>> {
        let Some(plugin) = crate::GLOBAL_PROVIDER.get() else {
            return Ok(Err("MysqlProvider not initialized — host.start() not called".to_string()));
        };
        let pool = match plugin.pools.get_or_create(&connection_url).await {
            Ok(p) => p,
            Err(e) => return Ok(Err(e.to_string())),
        };
        Ok(sqlx::query("SELECT 1")
            .execute(&pool)
            .await
            .map(|_| ())
            .map_err(|e: sqlx::Error| e.to_string()))
    }
}

// ── HostPlugin trait implementation ──────────────────────────────────────────

#[async_trait::async_trait]
impl HostPlugin for MysqlProvider {
    fn id(&self) -> &'static str {
        MysqlProvider::PLUGIN_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::from([WitInterface::from("graphily:mysql/mysql-api@0.1.0")]),
            exports: HashSet::new(),
        }
    }

    async fn start(&self) -> anyhow::Result<()> {
        let _ = crate::GLOBAL_PROVIDER.set(self.clone());
        tracing::info!(plugin = "graphily-mysql", "MySQL HostPlugin started and registered");
        Ok(())
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        _interfaces: HashSet<WitInterface>,
    ) -> anyhow::Result<()> {
        tracing::info!(plugin = "graphily-mysql", "Binding graphily:mysql/mysql-api to workload component");
        bindings::graphily::mysql::mysql_api::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;
        Ok(())
    }
}

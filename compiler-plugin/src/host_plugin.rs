//! wash-runtime `HostPlugin` implementation for `kompilre:compiler/compiler-api`.

use std::collections::HashSet;

use wash_runtime::{
    engine::ctx::{ActiveCtx, SharedCtx, extract_active_ctx},
    engine::workload::WorkloadItem,
    plugin::HostPlugin,
    wit::{WitInterface, WitWorld},
};

use crate::CompilerProvider;

// ── Wasmtime component bindings for the host side ────────────────────────────

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "compiler-host",
        imports: { default: async | trappable },
        wasmtime_crate: wash_runtime::wasmtime,
    });
}

// ── Host trait implementation ─────────────────────────────────────────────────

impl<'a> bindings::kompilre::compiler::compiler_api::Host for ActiveCtx<'a> {
    async fn compile_and_deploy(
        &mut self,
        files: Vec<bindings::kompilre::compiler::compiler_api::EntityFile>,
        registry_url: String,
        app_id: String,
        provision: Option<bindings::kompilre::compiler::compiler_api::ProvisionSpec>,
    ) -> wasmtime::Result<Result<String, String>> {
        tracing::info!(
            plugin = "kompilre-compiler",
            files = files.len(),
            registry = %registry_url,
            app_id = %app_id,
            provision = provision.is_some(),
            "compile-and-deploy called"
        );

        let Some(plugin) = crate::GLOBAL_PROVIDER.get() else {
            return Ok(Err("CompilerProvider not initialized — host.start() not called".to_string()));
        };

        let repo_root = plugin.repo_root.clone();
        // The spawn_blocking closure takes ownership, and the bindgen record is not `Send`
        // across that boundary in a useful way — copy it into the executor's own type.
        let provision_owned = provision.map(|p| crate::executor::Provision {
            host: p.host,
            db_url: p.db_url,
        });
        let entity_files: Vec<(String, String)> = files
            .into_iter()
            .map(|f| (f.name, f.content))
            .collect();

        let build_timeout_secs = std::env::var("COMPILER_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(300);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(build_timeout_secs),
            tokio::task::spawn_blocking(move || {
                crate::executor::compile_and_deploy(
                    &entity_files,
                    &repo_root,
                    &registry_url,
                    &app_id,
                    provision_owned.as_ref(),
                )
            }),
        )
        .await
        .map_err(|_| wasmtime::Error::msg(format!("compile_and_deploy timed out after {build_timeout_secs}s")))?
        .map_err(|e| wasmtime::Error::msg(e.to_string()))?;

        match &result {
            Ok(msg) => tracing::info!(plugin = "kompilre-compiler", msg = %msg, "compile-and-deploy succeeded"),
            Err(e)  => tracing::error!(plugin = "kompilre-compiler", err = %e, "compile-and-deploy failed"),
        }

        Ok(result)
    }
}

// ── HostPlugin trait implementation ──────────────────────────────────────────

#[async_trait::async_trait]
impl HostPlugin for CompilerProvider {
    fn id(&self) -> &'static str {
        CompilerProvider::PLUGIN_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            // Advertised in BOTH sets deliberately. Upstream plugins put a provided
            // capability in `imports` (wasi:keyvalue, wasi:config, blobstore, postgres
            // all do), while the nine built-in wasi interfaces sit in `exports` — so the
            // semantics are ambiguous here, and `log_interfaces` only ever prints
            // `exports`. Listing it in both removes the guess.
            imports: HashSet::from([WitInterface::from("kompilre:compiler/compiler-api@0.1.0")]),
            exports: HashSet::from([WitInterface::from("kompilre:compiler/compiler-api@0.1.0")]),
        }
    }

    async fn start(&self) -> anyhow::Result<()> {
        let _ = crate::GLOBAL_PROVIDER.set(self.clone());
        tracing::info!(
            plugin = "kompilre-compiler",
            repo_root = %self.repo_root,
            "Compiler HostPlugin started and registered"
        );
        Ok(())
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        _interfaces: HashSet<WitInterface>,
    ) -> anyhow::Result<()> {
        tracing::info!(plugin = "kompilre-compiler", "Binding kompilre:compiler/compiler-api to workload component");
        bindings::kompilre::compiler::compiler_api::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;
        Ok(())
    }
}

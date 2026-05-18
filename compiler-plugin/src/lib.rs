//! Graphily Compiler host plugin — wash-runtime HostPlugin for `kompilre:compiler/compiler-api`.
//!
//! Compiled into the wasmCloud fork's `wash host` binary alongside the MySQL plugin.
//! CompilerProvider implements HostPlugin and is registered via:
//!   ClusterHostBuilder::with_plugin(Arc::new(CompilerProvider::new()))

pub mod executor;
pub mod host_plugin;

pub(crate) static GLOBAL_PROVIDER: std::sync::OnceLock<CompilerProvider> =
    std::sync::OnceLock::new();

/// Compiler host plugin for `kompilre:compiler/compiler-api`.
/// Wired into the wasmCloud host in-process via the Wasmtime linker.
/// Receives entity .rs files from schema-compiler, runs wash build,
/// pushes to OCI, and deploys the WorkloadDeployment.
#[derive(Clone, Default)]
pub struct CompilerProvider {
    /// Root of the graphily repo — used to locate crates/entities/generated/
    pub repo_root: String,
}

impl CompilerProvider {
    pub const PLUGIN_ID: &'static str = "kompilre-compiler";

    pub fn new() -> Self {
        let repo_root = std::env::var("GRAPHILY_REPO_ROOT")
            .unwrap_or_else(|_| ".".to_string());
        Self { repo_root }
    }
}

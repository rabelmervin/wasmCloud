//! Native build pipeline: write entity files → wash build → OCI push → deploy.

use std::path::Path;
use std::process::Command;

/// Write .rs files, run wash build, push to OCI, deploy WorkloadDeployment.
/// Runs synchronously — caller wraps in spawn_blocking.
pub fn compile_and_deploy(
    files: &[(String, String)],
    repo_root: &str,
    registry_url: &str,
    image_tag: &str,
) -> Result<String, String> {
    let root = Path::new(repo_root);

    // ── Step 1: write entity .rs files ───────────────────────────────────────
    let generated_dir = root.join("crates/entities/generated");
    std::fs::create_dir_all(&generated_dir)
        .map_err(|e| format!("create generated dir: {e}"))?;

    for (name, content) in files {
        let path = generated_dir.join(name);
        std::fs::write(&path, content)
            .map_err(|e| format!("write {name}: {e}"))?;
        tracing::info!(plugin = "kompilre-compiler", file = %name, "wrote entity file");
    }

    // ── Step 1b: write schema.json to repo root ──────────────────────────────
    // proc macros (entities_macros::intr! / register_temporal_columns!) read
    // schema.json at compile time via the path "../../schema.json" relative to
    // crates/entities/ — which resolves to the repo root.
    // The schema-compiler WASM passes the schema JSON as a file named "schema.json".
    if let Some((_, schema_content)) = files.iter().find(|(n, _)| n == "schema.json") {
        let schema_path = root.join("schema.json");
        // Pretty-print for human readability; fall back to raw bytes if the
        // payload isn't valid JSON (the proc macros accept either form).
        let pretty = serde_json::from_str::<serde_json::Value>(schema_content)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or_else(|_| schema_content.clone());
        std::fs::write(&schema_path, pretty)
            .map_err(|e| format!("write schema.json to repo root: {e}"))?;
        tracing::info!(plugin = "kompilre-compiler", path = %schema_path.display(), "wrote schema.json to repo root");
    } else {
        tracing::warn!(plugin = "kompilre-compiler", "schema.json not found in entity files — proc macros may fail");
    }

    // ── Step 2: wash build ───────────────────────────────────────────────────
    let entities_crate = root.join("crates/entities");
    tracing::info!(plugin = "kompilre-compiler", path = %entities_crate.display(), "running wash build");

    let build_out = Command::new("wash")
        .args(["build", "--skip-fetch"])
        .current_dir(&entities_crate)
        .output()
        .map_err(|e| format!("wash build failed to start: {e}"))?;

    if !build_out.status.success() {
        let stderr = String::from_utf8_lossy(&build_out.stderr);
        return Err(format!("wash build failed:\n{stderr}"));
    }
    tracing::info!(plugin = "kompilre-compiler", "wash build succeeded");

    // ── Step 3: OCI push ─────────────────────────────────────────────────────
    // wasmcloud.toml destination = "../../build/entities.wasm" → relative to
    // crates/entities/, resolves to <repo_root>/build/entities.wasm
    let wasm_path = root.join("build/entities.wasm");
    if !wasm_path.exists() {
        return Err(format!("built wasm not found at {}", wasm_path.display()));
    }

    let image_name = "graphily/entities";
    let registry_host = registry_url.trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let image_ref  = format!("{registry_host}/{image_name}:{image_tag}");
    let layer_arg  = format!(
        "{}:application/vnd.module.wasm.content.layer.v1+wasm",
        wasm_path.display()
    );

    tracing::info!(plugin = "kompilre-compiler", image = %image_ref, "pushing to OCI");

    let push_out = Command::new("oras")
        .args(["push", &image_ref, &layer_arg, "--plain-http", "--disable-path-validation"])
        .output()
        .map_err(|e| format!("oras push failed to start: {e}"))?;

    if !push_out.status.success() {
        let stderr = String::from_utf8_lossy(&push_out.stderr);
        return Err(format!("oras push failed:\n{stderr}"));
    }
    tracing::info!(plugin = "kompilre-compiler", "OCI push succeeded");

    // ── Step 4: patch the entities component image in the existing WorkloadDeployment
    // Set SKIP_DEPLOY=1 to skip kubectl (useful when testing without k8s).
    if std::env::var("SKIP_DEPLOY").as_deref() == Ok("1") {
        tracing::info!(plugin = "kompilre-compiler", "SKIP_DEPLOY=1 — skipping kubectl patch");
        return Ok(format!("built and pushed {image_ref} (deploy skipped)"));
    }
    let wd_name      = std::env::var("GRAPHILY_WORKLOAD").unwrap_or_else(|_| "graphily".into());
    let wd_namespace = std::env::var("GRAPHILY_NAMESPACE").unwrap_or_else(|_| "graphily".into());
    let component_name = std::env::var("GRAPHILY_ENTITIES_COMPONENT")
        .unwrap_or_else(|_| "graphily-entities".into());

    // Use JSON patch (RFC 6902) to update only the matching component's image.
    // First fetch the current spec to find the component's array index.
    let get_out = Command::new("kubectl")
        .args(["get", "workloaddeployment", &wd_name, "-n", &wd_namespace, "-o", "json"])
        .output()
        .map_err(|e| format!("kubectl get failed to start: {e}"))?;

    if !get_out.status.success() {
        let stderr = String::from_utf8_lossy(&get_out.stderr);
        return Err(format!("kubectl get failed:\n{stderr}"));
    }

    let current: serde_json::Value = serde_json::from_slice(&get_out.stdout)
        .map_err(|e| format!("failed to parse workload JSON: {e}"))?;

    let mut current_mut = current.clone();
    let components = current_mut
        .pointer_mut("/spec/template/spec/components")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| "no components array in workload spec".to_string())?;

    if components.len() < 2 {
        return Err(format!(
            "workload spec has only {} component(s) — refusing to patch to avoid data loss",
            components.len()
        ));
    }

    let idx = components
        .iter()
        .position(|c| c.get("name").and_then(|n| n.as_str()) == Some(&component_name))
        .ok_or_else(|| format!("component '{component_name}' not found in workload spec"))?;

    components[idx]["image"] = serde_json::Value::String(image_ref.clone());

    let patch = serde_json::json!([{
        "op": "replace",
        "path": format!("/spec/template/spec/components/{idx}/image"),
        "value": image_ref,
    }]);

    let patch_str = serde_json::to_string(&patch).unwrap();

    tracing::info!(
        plugin = "kompilre-compiler",
        workload = %wd_name,
        namespace = %wd_namespace,
        component = %component_name,
        index = idx,
        image = %image_ref,
        total_components = components.len(),
        "patching WorkloadDeployment component image"
    );

    let deploy_out = Command::new("kubectl")
        .args([
            "patch", "workloaddeployment", &wd_name,
            "-n", &wd_namespace,
            "--type=json",
            &format!("--patch={patch_str}"),
        ])
        .output()
        .map_err(|e| format!("kubectl patch failed to start: {e}"))?;

    if !deploy_out.status.success() {
        let stderr = String::from_utf8_lossy(&deploy_out.stderr);
        return Err(format!("kubectl patch failed:\n{stderr}"));
    }

    tracing::info!(plugin = "kompilre-compiler", image = %image_ref, "entities hot-swapped in wasmCloud");
    Ok(format!("deployed {image_ref}"))
}

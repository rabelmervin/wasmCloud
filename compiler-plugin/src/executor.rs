//! Native build pipeline: write entity files → wash build → OCI push → deploy.

use kube::{
    api::{Api, ListParams, Patch, PatchParams},
    core::{DynamicObject, GroupVersionKind},
    discovery, Client,
};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

/// The label Kompilre finds an app's workload by. Written by the chart, and by
/// `provision_app` below when Kompilre creates the workload itself.
const APP_ID_LABEL: &str = "graphily.io/app-id";

/// Write .rs files, run wash build, push to OCI, deploy WorkloadDeployment.
/// Runs synchronously — caller wraps in spawn_blocking.
/// Hostname + database for an app that has no workload yet.
///
/// Present only on an onboarding request. A schema change carries neither, so the create
/// path below is unreachable from an ordinary compile — a bug in flow B cannot provision
/// a tenant.
#[derive(Debug, Clone)]
pub struct Provision {
    pub host: String,
    pub db_url: String,
}

/// Write `content` to `path` only when it differs from what is already there.
///
/// Cargo decides a source file is dirty by comparing mtime, not content, so rewriting a
/// byte-identical file still forces a full rebuild of entities-macros and entities —
/// measured at 250s, or 51s once codegen-units is raised. Since schema-compiler emits the
/// whole generated tree on every compile, most of those files are identical most of the
/// time. Skipping the write leaves mtime alone and lets cargo skip the crate entirely.
///
/// Returns true when the file was actually written.
fn write_if_changed(path: &Path, content: &str) -> Result<bool, String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == content {
            return Ok(false);
        }
    }
    std::fs::write(path, content)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(true)
}

pub fn compile_and_deploy(
    files: &[(String, String)],
    repo_root: &str,
    registry_url: &str,
    app_id: &str,
    provision: Option<&Provision>,
) -> Result<String, String> {
    if app_id.trim().is_empty() {
        return Err("compile_and_deploy called without an app_id".to_string());
    }
    let image_tag = {
        let (_, schema_content) = files
            .iter()
            .find(|(n,_)| n == "schema.json")
            .ok_or_else(|| "schema.json not found from entity files - cannot derive image tag".to_string())?;
        let mut hasher = Sha256::new();
        hasher.update(schema_content.as_bytes());
        let hex = format!("{:x}", hasher.finalize());
        hex[..12].to_string()
    };

    tracing::info!(
        plugin = "kompilre-compiler",
        app_id = %app_id,
        schema_hash = %image_tag,
        "compile-and-deploy start"
    );
    
    let root = Path::new(repo_root);

    // ── Step 1: write entity .rs files ───────────────────────────────────────
    let generated_dir = root.join("crates/entities/generated");
    std::fs::create_dir_all(&generated_dir)
        .map_err(|e| format!("create generated dir: {e}"))?;

    let mut changed_files = 0usize;
    for (name, content) in files {
        let path = generated_dir.join(name);
        if write_if_changed(&path, content)? {
            changed_files += 1;
            tracing::info!(plugin = "kompilre-compiler", file = %name, "wrote entity file");
        }
    }
    tracing::info!(
        plugin = "kompilre-compiler",
        changed = changed_files,
        total = files.len(),
        "entity files written (unchanged files left untouched so cargo can skip the rebuild)"
    );

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
        if write_if_changed(&schema_path, &pretty)? {
            tracing::info!(plugin = "kompilre-compiler", path = %schema_path.display(), "wrote schema.json to repo root");
        } else {
            tracing::info!(plugin = "kompilre-compiler", "schema.json unchanged — left untouched");
        }
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
        return Ok(image_ref);
    }
    let target = deploy_entities_image(app_id, &image_ref, repo_root, provision)?;

    tracing::info!(
        plugin = "kompilre-compiler",
        image = %image_ref,
        target = %target,
        "entities hot-swapped in wasmCloud"
    );
    // Return the bare image ref, not a sentence: the caller publishes it to
    // `entities.ready` so downstream orchestration (the provisioner) can act on it.
    Ok(image_ref)
}

/// Point `app_id`'s WorkloadDeployment at `image_ref`, returning `namespace/name`.
///
/// Talks to the Kubernetes API directly rather than shelling out to `kubectl`: no
/// external binary on the build host, typed errors instead of parsed stderr, and
/// the credential is whatever the client is configured with rather than whatever
/// kubeconfig happened to be in the process environment.
///
/// Sync wrapper around an async client. `compile_and_deploy` runs inside
/// `spawn_blocking`, so a dedicated current-thread runtime keeps these calls
/// self-contained — `Handle::block_on` from a blocking thread can deadlock the
/// runtime that owns it.
fn deploy_entities_image(
    app_id: &str,
    image_ref: &str,
    repo_root: &str,
    provision: Option<&Provision>,
) -> Result<String, String> {
    let selector = format!("{APP_ID_LABEL}={app_id}");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build runtime for k8s client: {e}"))?;

    rt.block_on(async {
        // Uses the in-cluster ServiceAccount token when running as a pod, and falls
        // back to the ambient kubeconfig otherwise. Outside the cluster, KUBECONFIG
        // MUST point at a kubeconfig bound to the scoped `kompilre-deployer`
        // ServiceAccount — otherwise this inherits whatever rights the operator has.
        let client = Client::try_default()
            .await
            .map_err(|e| format!("k8s client init failed: {e}"))?;

        let gvk = GroupVersionKind::gvk("runtime.wasmcloud.dev", "v1alpha1", "WorkloadDeployment");
        let (ar, _caps) = discovery::pinned_kind(&client, &gvk)
            .await
            .map_err(|e| format!("WorkloadDeployment CRD not discoverable: {e}"))?;

        // Find this app's workload by label. Fail closed: exactly one match, or
        // error. The old GRAPHILY_WORKLOAD default meant an unrecognised app_id
        // silently deployed onto whichever workload was named "graphily".
        let all: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
        let list = all
            .list(&ListParams::default().labels(&selector))
            .await
            .map_err(|e| format!("listing workloaddeployments failed: {e}"))?;

        let workload = match (list.items.len(), provision) {
            (1, _) => &list.items[0],

            // Onboarding: no workload yet, and the caller supplied a hostname and database.
            (0, Some(spec)) => {
                // The router resolves requests by Host header to a *set* of workloads and
                // takes an arbitrary member, so a duplicate hostname silently cross-serves
                // tenants. Nothing downstream detects it — this is the only place it can be
                // caught.
                let all_wl = all.list(&ListParams::default()).await.map_err(|e| {
                    format!("listing workloaddeployments for hostname check failed: {e}")
                })?;
                if let Some(clash) = all_wl.items.iter().find(|w| {
                    w.metadata.labels.as_ref().and_then(|l| l.get(APP_ID_LABEL))
                        != Some(&app_id.to_string())
                        && workload_hosts(w).iter().any(|h| h == &spec.host)
                }) {
                    return Err(format!(
                        "hostname '{}' is already claimed by {} — refusing to provision",
                        spec.host,
                        clash.metadata.name.as_deref().unwrap_or("<unnamed>")
                    ));
                }

                let hash = image_ref.rsplit(':').next().unwrap_or_default();
                let created = provision_app(&client, repo_root, app_id, spec, hash).await?;
                tracing::info!(
                    plugin = "kompilre-compiler",
                    app_id = %app_id,
                    host = %spec.host,
                    workload = %created,
                    "provisioned a new app"
                );
                // The workload was created already pinned to this image, so there is
                // nothing left to patch.
                return Ok(created);
            }

            (0, None) => {
                return Err(format!(
                    "no workload labelled graphily.io/app-id={app_id} — refusing to deploy"
                ));
            }
            (n, _) => {
                return Err(format!(
                    "{n} workloads claim graphily.io/app-id={app_id} — refusing to deploy"
                ));
            }
        };

        let wd_name = workload
            .metadata
            .name
            .clone()
            .ok_or_else(|| "workload has no metadata.name".to_string())?;
        let wd_namespace = workload
            .metadata
            .namespace
            .clone()
            .ok_or_else(|| "workload has no metadata.namespace".to_string())?;

        // `data` carries everything outside metadata, so the spec lives there.
        let components = workload
            .data
            .pointer("/spec/template/spec/components")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "no components array in workload spec".to_string())?;

        if components.len() < 2 {
            return Err(format!(
                "workload spec has only {} component(s) — refusing to patch to avoid data loss",
                components.len()
            ));
        }

        // Match on the image repository rather than the component name: the name is
        // chosen by whoever provisions the workload, the repository is chosen here.
        let idx = components
            .iter()
            .position(|c| {
                c.get("image")
                    .and_then(|i| i.as_str())
                    .is_some_and(|i| i.contains("graphily/entities"))
            })
            .ok_or_else(|| format!("no entities component in workload '{wd_name}'"))?;

        let component_name = components[idx]
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_string();

        // `test` makes the patch atomic against the components array being reordered
        // between this read and the write below: if index `idx` is no longer that
        // component, the API server rejects the whole patch instead of overwriting a
        // different one.
        let patch_value = serde_json::json!([
            { "op": "test",
              "path": format!("/spec/template/spec/components/{idx}/name"),
              "value": component_name },
            { "op": "replace",
              "path": format!("/spec/template/spec/components/{idx}/image"),
              "value": image_ref },
        ]);
        let patch: json_patch::Patch = serde_json::from_value(patch_value)
            .map_err(|e| format!("failed to build json patch: {e}"))?;

        tracing::info!(
            plugin = "kompilre-compiler",
            app_id = %app_id,
            workload = %wd_name,
            namespace = %wd_namespace,
            component = %component_name,
            index = idx,
            image = %image_ref,
            total_components = components.len(),
            "patching WorkloadDeployment component image"
        );

        let ns: Api<DynamicObject> = Api::namespaced_with(client, &wd_namespace, &ar);
        ns.patch(
            &wd_name,
            &PatchParams::apply("kompilre-compiler"),
            &Patch::Json::<()>(patch),
        )
        .await
        .map_err(|e| format!("patch failed: {e}"))?;

        Ok(format!("{wd_namespace}/{wd_name}"))
    })
}

/// Every Host header a workload claims, from its `wasi:http/incoming-handler` config.
fn workload_hosts(w: &DynamicObject) -> Vec<String> {
    w.data
        .pointer("/spec/template/spec/hostInterfaces")
        .and_then(|v| v.as_array())
        .map(|ifaces| {
            ifaces
                .iter()
                .filter_map(|i| i.pointer("/config/host").and_then(|h| h.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Create an app's Secret, ConfigMap and WorkloadDeployment, pinned to `hash`.
///
/// The manifests come from `helm template` rather than being built here, so the chart in
/// `k8s/chart` stays the single definition of a workload — used identically by a developer
/// running helm by hand and by this function in production. `helm template` is a pure
/// render with no cluster access; only the applies below are privileged.
///
/// Applied with server-side apply, so re-running onboarding for an existing app is a no-op
/// rather than an `AlreadyExists` error — the caller may safely retry.
async fn provision_app(
    client: &Client,
    repo_root: &str,
    app_id: &str,
    spec: &Provision,
    hash: &str,
) -> Result<String, String> {
    // A DNS-safe short name for the Kubernetes object names; the app_id remains the
    // authoritative identifier and lives on the label.
    let short: String = app_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(20)
        .collect::<String>()
        .to_lowercase();
    let release = format!("app-{short}");
    let chart = Path::new(repo_root).join("k8s/chart");

    let out = Command::new("helm")
        .args([
            "template",
            &release,
            &chart.to_string_lossy(),
            "--set", &format!("app.name={short}"),
            "--set", &format!("app.id={app_id}"),
            "--set", &format!("app.host={}", spec.host),
            "--set", &format!("app.schemaHash={hash}"),
            "--set", &format!("app.dbUrl={}", spec.db_url),
        ])
        .output()
        .map_err(|e| format!("helm template failed to start: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "helm template failed:
{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let rendered = String::from_utf8_lossy(&out.stdout);

    let mut applied = 0usize;
    let mut workload_ref = String::new();

    for doc in rendered.split("
---") {
        if doc.trim().is_empty() {
            continue;
        }
        let obj: DynamicObject = match serde_yaml_ng::from_str(doc) {
            Ok(o) => o,
            Err(e) => return Err(format!("could not parse a rendered manifest: {e}")),
        };
        let Some(types) = obj.types.clone() else {
            return Err("a rendered manifest has no apiVersion/kind".to_string());
        };
        let name = obj
            .metadata
            .name
            .clone()
            .ok_or_else(|| "a rendered manifest has no metadata.name".to_string())?;
        let ns = obj
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| "graphily".to_string());

        let gvk = GroupVersionKind::try_from(types)
            .map_err(|e| format!("unrecognised apiVersion/kind: {e}"))?;
        let (ar, _caps) = discovery::pinned_kind(client, &gvk)
            .await
            .map_err(|e| format!("{} is not discoverable: {e}", gvk.kind))?;
        let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), &ns, &ar);

        api.patch(
            &name,
            &PatchParams::apply("kompilre-compiler").force(),
            &Patch::Apply(&obj),
        )
        .await
        .map_err(|e| format!("applying {} {ns}/{name} failed: {e}", gvk.kind))?;

        tracing::info!(
            plugin = "kompilre-compiler",
            kind = %gvk.kind,
            name = %name,
            namespace = %ns,
            "applied"
        );
        applied += 1;
        if gvk.kind == "WorkloadDeployment" {
            workload_ref = format!("{ns}/{name}");
        }
    }

    if workload_ref.is_empty() {
        return Err(format!(
            "{applied} manifest(s) applied but none was a WorkloadDeployment"
        ));
    }
    Ok(workload_ref)
}

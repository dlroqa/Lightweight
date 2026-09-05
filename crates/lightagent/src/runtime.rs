//! `lightagent runtime` — report the engine's device and place models.
//!
//! A thin surface over `lightagent-runtime`, which speaks the gateway's control
//! plane. `show` reads (the engine's device and capabilities, the machine, and
//! the model catalog) and never changes anything. `place` swaps the resident
//! model, so it is an explicit action, gated on the operator running it — it is
//! never a side effect of a chat turn.

use lightagent_core::{Config, ConfigStore, LightagentPaths};
use lightagent_runtime::{
    LoadPlan, PlacementPolicy, RuntimeClient, RuntimeEndpoint, placement::resolve_device,
};

/// Load the config from the resolved home.
fn load_config() -> Result<Config, String> {
    let paths = LightagentPaths::resolve().map_err(|error| error.to_string())?;
    ConfigStore::at(&paths)
        .load()
        .map_err(|error| error.to_string())
}

/// Build the control-plane endpoint from the inference config.
fn endpoint(config: &Config) -> RuntimeEndpoint {
    let mut endpoint = RuntimeEndpoint::new(config.inference.base_url.clone());
    if let Some(secret) = &config.inference.api_key
        && let Some(value) = secret.resolve()
    {
        endpoint = endpoint.with_api_key(value);
    }
    endpoint
}

/// Build the placement policy from the runtime config.
fn policy(config: &Config) -> PlacementPolicy {
    PlacementPolicy {
        preferred_device: config.runtime.preferred_device.clone(),
        allow_cpu_fallback: config.runtime.allow_cpu_fallback,
        n_ctx: config.runtime.n_ctx,
        threads: config.runtime.threads,
        kv_type: config.runtime.kv_type.clone(),
        load_mode: config.runtime.load_mode.clone(),
        ubatch: config.runtime.ubatch,
    }
}

fn client(config: &Config) -> Result<RuntimeClient, String> {
    RuntimeClient::new(endpoint(config)).map_err(|error| error.to_string())
}

/// `runtime show` — the engine's device, capabilities, the machine and catalog.
pub async fn show(json: bool) -> Result<(), String> {
    let config = load_config()?;
    let client = client(&config)?;
    let policy = policy(&config);

    let gateway = client.gateway().await.map_err(|error| {
        format!(
            "could not read the gateway at {}: {error}",
            config.inference.base_url
        )
    })?;
    // The system probe and catalog are best-effort: a gateway can answer while a
    // probe is unavailable, and a report is still useful without them.
    let system = client.system().await.ok();
    let catalog = client.catalog().await.unwrap_or_default();

    let resolution = resolve_device(&policy, &gateway.engine_capabilities);

    if json {
        let value = serde_json::json!({
            "base_url": config.inference.base_url,
            "backend": gateway.backend,
            "engine_build": gateway.engine_capabilities.build,
            "resident_model": gateway.model,
            "device": gateway.engine_capabilities.device,
            "preferred_device": config.runtime.preferred_device,
            "resolved": match &resolution {
                Ok(r) => serde_json::json!({
                    "device": r.device.as_str(),
                    "fell_back_from": r.fell_back_from.map(|d| d.as_str()),
                    "summary": r.summary(),
                }),
                Err(error) => serde_json::json!({ "error": error.to_string() }),
            },
            "capabilities": {
                "streaming": gateway.engine_capabilities.streaming,
                "tool_calls": gateway.engine_capabilities.tool_calls,
                "reasoning_content": gateway.engine_capabilities.reasoning_content,
                "max_concurrent_requests": gateway.engine_capabilities.max_concurrent_requests,
                "kv_cache_types": gateway.engine_capabilities.kv_cache_types,
            },
            "defaults": {
                "kv_type": gateway.defaults.kv_type,
                "threads": gateway.defaults.threads,
                "ubatch": gateway.defaults.ubatch,
                "load_modes": gateway.defaults.load_modes,
            },
            "system": system.as_ref().map(|s| serde_json::json!({
                "os": s.os.name,
                "architecture": s.os.architecture,
                "logical_cores": s.cpu.logical_cores,
                "has_avx_family": s.cpu.has_avx_family,
                "memory_total": s.memory.total,
                "memory_available": s.memory.available,
            })),
            "models": catalog.iter().map(|m| serde_json::json!({
                "id": m.id,
                "state": m.state,
                "supported": m.supported,
            })).collect::<Vec<_>>(),
        });
        println!("{value:#}");
        return Ok(());
    }

    let mut out = String::new();
    out.push_str(&format!(
        "Engine:    {} @ {}\n",
        gateway.backend.as_deref().unwrap_or("(unknown)"),
        config.inference.base_url
    ));
    if let Some(build) = &gateway.engine_capabilities.build {
        out.push_str(&format!("Build:     {build}\n"));
    }
    out.push_str(&format!(
        "Device:    {} (engine reports {:?})\n",
        match &resolution {
            Ok(r) => r.summary(),
            Err(error) => error.to_string(),
        },
        gateway.engine_capabilities.device
    ));
    out.push_str(&format!("Preferred: {}\n", config.runtime.preferred_device));
    out.push_str(&format!(
        "Resident:  {}\n",
        gateway.model.as_deref().unwrap_or("(none loaded)")
    ));
    out.push_str(&format!(
        "KV types:  {}\n",
        if gateway.engine_capabilities.kv_cache_types.is_empty() {
            "(none reported)".to_owned()
        } else {
            gateway.engine_capabilities.kv_cache_types.join(", ")
        }
    ));
    if let Some(system) = &system {
        let cores = system
            .cpu
            .logical_cores
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_owned());
        let avx = match system.cpu.has_avx_family {
            Some(true) => " with AVX",
            Some(false) => " without AVX",
            None => "",
        };
        out.push_str(&format!(
            "Machine:   {} {}, {cores} logical cores{avx}\n",
            system.os.name.as_deref().unwrap_or("?"),
            system.os.architecture.as_deref().unwrap_or("?"),
        ));
        if let (Some(total), Some(available)) = (system.memory.total, system.memory.available) {
            out.push_str(&format!(
                "Memory:    {:.1} GiB available of {:.1} GiB\n",
                available as f64 / 1024.0 / 1024.0 / 1024.0,
                total as f64 / 1024.0 / 1024.0 / 1024.0,
            ));
        }
    }
    if catalog.is_empty() {
        out.push_str("Models:    (none in the catalog)\n");
    } else {
        out.push_str("Models:\n");
        for model in &catalog {
            out.push_str(&format!("  {:<10} {}\n", model.state, model.id));
        }
    }
    print!("{out}");
    Ok(())
}

/// `runtime place <model>` — load a model with the configured runtime params.
///
/// Mutating: it swaps what the engine is serving. It first resolves the device
/// policy, so a `preferred_device` the engine cannot honour with fallback off
/// refuses here rather than silently loading on the wrong device.
pub async fn place(model: String, force: bool, json: bool) -> Result<(), String> {
    let config = load_config()?;
    let client = client(&config)?;
    let policy = policy(&config);

    let gateway = client.gateway().await.map_err(|error| {
        format!(
            "could not read the gateway at {}: {error}",
            config.inference.base_url
        )
    })?;
    let resolution =
        resolve_device(&policy, &gateway.engine_capabilities).map_err(|error| error.to_string())?;

    let mut plan = LoadPlan::from_policy(&policy);
    plan.force = force;
    let outcome = client
        .place(&model, &plan)
        .await
        .map_err(|error| error.to_string())?;

    if json {
        let value = serde_json::json!({
            "model": outcome.model,
            "job": outcome.job,
            "device": resolution.device.as_str(),
            "fell_back_from": resolution.fell_back_from.map(|d| d.as_str()),
        });
        println!("{value:#}");
    } else {
        println!("Placing {} — {}.", outcome.model, resolution.summary());
        if let Some(job) = &outcome.job {
            println!("  load job: {job}");
        }
        println!("Run `lightagent runtime show` to confirm it is resident.");
    }
    Ok(())
}

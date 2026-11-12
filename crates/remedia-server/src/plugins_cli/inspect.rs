//! `plugins list` and `plugins inspect`: configuration views, with optional live checks.
use remedia_server_config::PluginHostConfig;
use remedia_server_config::error::{PluginError, PluginErrorKind};
use remedia_server_config::protocol::Manifest;
use serde_json::{Value, json};

use super::discovery::{discover, explain, resolve};
use super::prompt::table;
use super::session::{Loaded, Session};
use super::{Failure, InspectArgs, ListArgs, host_ids, raw_hosts};

fn endpoint_of(host: &Value) -> String {
    host.get("url")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned()
}

fn is_enabled(host: &Value) -> bool {
    host.get("enabled").and_then(Value::as_bool).unwrap_or(true)
}

fn operations_of(host: &Value) -> Vec<(String, String, bool)> {
    host.get("operations")
        .and_then(Value::as_array)
        .map(|operations| {
            operations
                .iter()
                .map(|operation| {
                    if let Some(name) = operation.as_str() {
                        let (plugin, operation) =
                            name.split_once('/').expect("validated operation");
                        return (plugin.to_owned(), operation.to_owned(), false);
                    }

                    (
                        operation
                            .get("pluginId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        operation
                            .get("operation")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        operation
                            .get("replaySafe")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Describe authentication without any secret: only whether a token exists and where it comes from.
fn auth_of(host: &Value) -> Value {
    let Some(auth) = host
        .get("auth")
        .filter(|auth| auth.get("type").and_then(Value::as_str) == Some("bearer"))
    else {
        return json!({"type": "none"});
    };
    let token = auth
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match token
        .strip_prefix("env('")
        .and_then(|rest| rest.strip_suffix("')"))
    {
        Some(variable) => json!({"type": "bearer", "tokenSource": "env", "tokenEnv": variable}),
        None => json!({"type": "bearer", "tokenSource": "inline"}),
    }
}

fn auth_label(auth: &Value) -> String {
    match auth["tokenSource"].as_str() {
        Some("env") => format!("bearer (env {})", auth["tokenEnv"].as_str().unwrap_or("?")),
        Some(_) => "bearer (inline in file)".to_owned(),
        None => "none".to_owned(),
    }
}

pub fn list(session: &Session, args: &ListArgs) -> Result<u8, Failure> {
    let loaded = session.load()?;
    let hosts = raw_hosts(loaded.text.as_str());

    if args.json {
        let hosts: Vec<Value> = hosts
            .iter()
            .map(|host| {
                json!({
                    "id": host.get("id"),
                    "url": endpoint_of(host),
                    "enabled": is_enabled(host),
                    "auth": auth_of(host),
                    "operations": operations_of(host).len(),
                    "health": "unverified",
                })
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "config": session.config_path.display().to_string(),
                "hosts": hosts,
            }))
            .map_err(|error| Failure::new(error.to_string()))?
        );

        return Ok(0);
    }

    if hosts.is_empty() {
        println!(
            "No plugin hosts are configured in {}.",
            session.config_path.display()
        );
        println!("Register one with: remedia-server plugins install <url>");

        return Ok(0);
    }

    let rows: Vec<Vec<String>> = hosts
        .iter()
        .map(|host| {
            vec![
                host.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned(),
                endpoint_of(host),
                if is_enabled(host) { "yes" } else { "no" }.to_owned(),
                operations_of(host).len().to_string(),
                auth_label(&auth_of(host)),
            ]
        })
        .collect();

    table(&["ID", "ENDPOINT", "ENABLED", "OPERATIONS", "AUTH"], &rows);
    println!();
    println!("This is configuration only; it does not show runtime health.");
    println!("Run `remedia-server plugins inspect <id> --check` to contact a host.");

    Ok(0)
}

/// Result of contacting one host.
pub enum Check {
    Reached(Manifest),
    /// The host's own configuration could not be resolved (for example an unset token variable).
    NotChecked(String),
    Failed(PluginError, bool),
}

/// Contact a configured host using its own credentials and the shared transport.
pub async fn check_host(session: &Session, entry: &Value, id: &str) -> Check {
    let config: PluginHostConfig = match resolve(session, entry) {
        Ok(config) => config,
        Err(failure) => return Check::NotChecked(failure.to_string()),
    };

    match discover(&config, Some(id)).await {
        Ok(manifest) => Check::Reached(manifest),
        Err(error) => Check::Failed(error, config.token.is_some()),
    }
}

/// Differences between a host's allowlist and its manifest.
pub struct Difference {
    /// Configured but no longer advertised.
    pub missing: Vec<(String, String)>,
    /// Advertised but not configured. Never enabled automatically.
    pub new: Vec<(String, String)>,
}

pub fn compare(configured: &[(String, String, bool)], manifest: &Manifest) -> Difference {
    let advertised = manifest.operations();

    Difference {
        missing: configured
            .iter()
            .filter(|(plugin, operation, _)| {
                !advertised.contains(&(plugin.clone(), operation.clone()))
            })
            .map(|(plugin, operation, _)| (plugin.clone(), operation.clone()))
            .collect(),
        new: advertised
            .into_iter()
            .filter(|pair| {
                !configured
                    .iter()
                    .any(|(plugin, operation, _)| (plugin, operation) == (&pair.0, &pair.1))
            })
            .collect(),
    }
}

pub fn configured_operations(entry: &Value) -> Vec<(String, String, bool)> {
    operations_of(entry)
}

pub fn failure_kind(error: &PluginError) -> &'static str {
    match error.kind {
        PluginErrorKind::Unauthorized => "authentication_rejected",
        PluginErrorKind::Incompatible => "incompatible",
        PluginErrorKind::Timeout => "timeout",
        _ => "unreachable",
    }
}

pub async fn run(session: &Session, args: &InspectArgs) -> Result<u8, Failure> {
    let loaded: Loaded = session.load()?;
    let entry = raw_hosts(loaded.text.as_str())
        .into_iter()
        .find(|host| host.get("id").and_then(Value::as_str) == Some(args.host_id.as_str()))
        .ok_or_else(|| {
            Failure::new(format!(
                "no plugin host `{}` is configured. Configured hosts: {}",
                args.host_id,
                host_ids(loaded.text.as_str()).join(", ")
            ))
        })?;
    let settings = loaded
        .settings
        .plugin_hosts
        .iter()
        .find(|host| host.id == args.host_id)
        .expect("a validated host is present");
    let operations = operations_of(&entry);
    let limits = &settings.limits;
    let configuration = json!({
        "id": args.host_id,
        "url": endpoint_of(&entry),
        "enabled": is_enabled(&entry),
        "auth": auth_of(&entry),
        "security": {
            "allowHttp": settings.allow_http,
            "allowExternalMedia": settings.allow_external_media,
            "mediaOrigins": settings.media_origins,
        },
        "limits": {
            "requestTimeoutMs": limits.request_timeout.as_millis() as u64,
            "discoveryTtlMs": limits.discovery_ttl.as_millis() as u64,
            "maxRequestBytes": limits.max_request_bytes,
            "maxResponseBytes": limits.max_response_bytes,
            "maxPendingOperations": limits.max_pending_operations,
            "maxActiveStreams": limits.max_active_streams,
            "maxMediaBytes": limits.max_media_bytes,
            "mediaIdleTimeoutMs": limits.media_idle_timeout.as_millis() as u64,
            "mediaBufferBytes": limits.media_buffer_bytes,
            "maxAttempts": limits.max_attempts,
        },
        "operations": operations
            .iter()
            .map(|(plugin, operation, replay_safe)| json!({"pluginId": plugin, "operation": operation, "replaySafe": replay_safe}))
            .collect::<Vec<_>>(),
    });
    let mut report = json!({"config": configuration});
    let mut problems = false;

    if args.check {
        let check = check_host(session, &entry, &args.host_id).await;
        let outcome = match &check {
            Check::Reached(manifest) => {
                let difference = compare(&operations, manifest);

                problems = !difference.missing.is_empty();

                json!({
                    "status": if problems { "operations_missing" } else { "ok" },
                    "protocolCompatible": true,
                    "authentication": if settings.token.is_some() { "accepted" } else { "none" },
                    "advertisedOperations": manifest.operation_count(),
                    "missingOperations": difference.missing.iter().map(|(p, o)| format!("{p}/{o}")).collect::<Vec<_>>(),
                    "unconfiguredOperations": difference.new.len(),
                })
            }
            Check::NotChecked(reason) => {
                problems = true;

                json!({"status": "not_checked", "reason": reason})
            }
            Check::Failed(error, has_token) => {
                problems = true;

                json!({
                    "status": failure_kind(error),
                    "protocolCompatible": error.kind != PluginErrorKind::Incompatible,
                    "authentication": if error.kind == PluginErrorKind::Unauthorized { "rejected" } else { "unknown" },
                    "reason": explain(error, *has_token),
                })
            }
        };

        report["check"] = outcome;
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| Failure::new(error.to_string()))?
        );

        return Ok(u8::from(problems) * 2);
    }

    let config = &report["config"];

    println!("Host:      {}", args.host_id);
    println!("Endpoint:  {}", config["url"].as_str().unwrap_or("?"));
    println!(
        "Enabled:   {}",
        if is_enabled(&entry) { "yes" } else { "no" }
    );
    println!("Auth:      {}", auth_label(&config["auth"]));
    println!(
        "Security:  HTTP {}, external media {}",
        yes_no(settings.allow_http),
        yes_no(settings.allow_external_media)
    );
    println!(
        "Limits:    timeout {} ms, {} pending, {} attempt(s), {} response bytes",
        config["limits"]["requestTimeoutMs"],
        limits.max_pending_operations,
        limits.max_attempts,
        limits.max_response_bytes
    );
    println!();

    let rows: Vec<Vec<String>> = operations
        .iter()
        .map(|(plugin, operation, replay_safe)| {
            vec![
                plugin.clone(),
                operation.clone(),
                yes_no(*replay_safe).to_owned(),
            ]
        })
        .collect();

    table(&["PLUGIN", "OPERATION", "REPLAY-SAFE"], &rows);

    if args.check {
        let check = &report["check"];

        println!();
        println!("Live check: {}", check["status"].as_str().unwrap_or("?"));

        if let Some(reason) = check["reason"].as_str() {
            println!("  {reason}");
        }

        if let Some(missing) = check["missingOperations"].as_array() {
            println!(
                "  protocol compatible, {} operation(s) advertised",
                check["advertisedOperations"]
            );

            for operation in missing {
                println!(
                    "  allowed but not advertised: {}",
                    operation.as_str().unwrap_or("?")
                );
            }

            if check["unconfiguredOperations"].as_u64().unwrap_or(0) > 0 {
                println!(
                    "  {} advertised operation(s) are not allowed; run `plugins sync {}` to review",
                    check["unconfiguredOperations"], args.host_id
                );
            }
        }
    } else {
        println!();
        println!("Configuration only. Add --check to contact the host.");
    }

    Ok(u8::from(problems) * 2)
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

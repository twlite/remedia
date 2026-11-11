//! Discovery of a plugin host through the shared, policy-checked transport.
use remedia_server_config::error::{PluginError, PluginErrorKind};
use remedia_server_config::protocol::Manifest;
use remedia_server_config::transport::{build_client, fetch_manifest_for};
use remedia_server_config::{Env, PluginHostConfig};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::Failure;
use super::session::{Session, describe};

/// The placeholder allowlist entry. Discovery only needs transport settings, but a host
/// registration must contain at least one operation to be valid.
const PLACEHOLDER: &str = "discovery";

/// Build transport settings for a host which is not registered yet, using the same
/// validation as a configuration file. The token is resolved from the environment only
/// here and is never written anywhere.
pub fn candidate(
    session: &Session,
    endpoint: &str,
    token_env: Option<&str>,
    allow_insecure_http: bool,
    allow_private_network: bool,
) -> Result<PluginHostConfig, Failure> {
    let mut entry = json!({
        "id": "candidate",
        "endpoint": endpoint,
        "operations": [{"pluginId": PLACEHOLDER, "operation": PLACEHOLDER}],
    });

    if allow_insecure_http {
        entry["allowInsecureHttp"] = json!(true);
    }

    if allow_private_network {
        entry["allowPrivateNetwork"] = json!(true);
    }

    if let Some(variable) = token_env {
        entry["auth"] = json!({"type": "bearer", "token": format!("env('{variable}')")});
    }

    resolve(session, &entry).map_err(|failure| {
        if failure.message.contains("environment variable") && token_env.is_some() {
            Failure::new(format!(
                "{} Export it, or pass --env-file/--optional-env-file with a file that defines it.",
                failure.message
            ))
        } else {
            failure
        }
    })
}

/// Transport settings for an entry already in the configuration, with strict resolution of
/// its own environment references.
pub fn resolve(session: &Session, entry: &serde_json::Value) -> Result<PluginHostConfig, Failure> {
    let document = json!({
        "users": [{"username": "remedia", "password": "remedia"}],
        "pluginHosts": [entry],
    });
    let mut env = Env::strict(|name| session.lookup(name));
    let settings =
        remedia_server_config::parse(&document.to_string(), std::path::Path::new("."), &mut env)
            .map_err(|error| Failure::new(hint(describe(&error))))?;

    Ok(settings
        .plugin_hosts
        .into_iter()
        .next()
        .expect("one host was supplied")
        .as_ref()
        .clone())
}

fn hint(message: String) -> String {
    if message.contains("must use https") {
        format!("{message}. Pass --allow-insecure-http to permit cleartext HTTP for this host.")
    } else if message.contains("private or loopback") {
        format!("{message}. Pass --allow-private-network to permit it for this host.")
    } else if message.contains("references environment variable") {
        format!("{message}.")
    } else {
        message
    }
}

/// Fetch `/.well-known/remedia`. Redirects, oversized bodies, timeouts, private addresses
/// and unsafe schemes are rejected by the shared transport.
pub async fn discover(
    config: &PluginHostConfig,
    expected_id: Option<&str>,
) -> Result<Manifest, PluginError> {
    let client = build_client(config)?;
    let token = CancellationToken::new();

    fetch_manifest_for(&client, config, expected_id, &token, &token).await
}

/// Explain a discovery failure without exposing credentials or URLs.
pub fn explain(error: &PluginError, has_token: bool) -> String {
    match error.kind {
        PluginErrorKind::Unauthorized if has_token => {
            "the host rejected the configured bearer token".to_owned()
        }
        PluginErrorKind::Unauthorized => {
            "the host requires authentication. Rerun with --token-env <VARIABLE>".to_owned()
        }
        PluginErrorKind::Incompatible => format!("{} (not a Remedia plugin host?)", error.detail),
        _ => error.detail.to_owned(),
    }
}

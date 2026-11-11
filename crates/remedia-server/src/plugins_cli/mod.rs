//! `remedia plugins ...`: manage the `pluginHosts` section of `remedia.config.json`.
//!
//! These commands register remote plugin-host deployments; they never install or run plugin
//! code. They depend only on `remedia-server-config`, so they work without FFmpeg and never
//! start the server, the playback runtime or any Discord connection.
mod discovery;
mod inspect;
mod install;
mod prompt;
mod session;
mod sync;
mod toggle;
mod uninstall;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand};
use remedia_server_config::edit::Json;

use session::Session;

const RESTART_NOTICE: &str =
    "Restart remedia-server for this change to take effect. A running server is not modified.";

/// A user-facing failure. The message never contains credentials.
#[derive(Debug)]
pub struct Failure {
    message: String,
    code: u8,
}

impl Failure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// Register a remote plugin host after inspecting its manifest.
    Install(InstallArgs),
    /// Remove a host registration. The remote service is not contacted.
    Uninstall(UninstallArgs),
    /// List configured hosts (offline).
    List(ListArgs),
    /// Show a host's configuration and, with --check, its live status.
    Inspect(InspectArgs),
    /// Enable a disabled host.
    Enable(ToggleArgs),
    /// Disable a host without deleting its configuration.
    Disable(ToggleArgs),
    /// Compare configured operations with what hosts currently advertise.
    Sync(SyncArgs),
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Origin of the plugin host, e.g. https://plugins.example.com
    pub url: String,
    /// Host ID to configure. Must equal the manifest's hostId when the host declares one.
    #[arg(long)]
    pub id: Option<String>,
    /// Read the bearer token from this environment variable. Only the reference is stored.
    #[arg(long, value_name = "VARIABLE")]
    pub token_env: Option<String>,
    /// Allow an advertised operation (PLUGIN/OPERATION). Repeatable.
    #[arg(long = "operation", value_name = "PLUGIN/OPERATION")]
    pub operations: Vec<String>,
    /// Allow every operation the host advertises. Acknowledges the full list.
    #[arg(long)]
    pub all_operations: bool,
    /// Mark an allowed operation as safe to retry (PLUGIN/OPERATION). Repeatable.
    #[arg(long = "replay-safe", value_name = "PLUGIN/OPERATION")]
    pub replay_safe: Vec<String>,
    /// Permit a cleartext http:// endpoint for this host.
    #[arg(long)]
    pub allow_insecure_http: bool,
    /// Permit a loopback or private-network endpoint for this host.
    #[arg(long)]
    pub allow_private_network: bool,
    /// Confirm without prompting. Does not bypass any security restriction.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Show the resulting entry without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Configured host ID.
    pub host_id: String,
    /// Confirm without prompting.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Show what would be removed without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Emit stable machine-readable JSON. Credentials are never included.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Configured host ID.
    pub host_id: String,
    /// Contact the host and report compatibility, authentication and missing operations.
    #[arg(long)]
    pub check: bool,
    /// Emit stable machine-readable JSON. Credentials are never included.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ToggleArgs {
    /// Configured host ID.
    pub host_id: String,
    /// Show the change without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Limit synchronization to one host. Default: every configured host.
    pub host_id: Option<String>,
    /// Report differences only. This is the default and writes nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Apply approved changes. Interactive sessions are asked about each change.
    #[arg(long)]
    pub apply: bool,
    /// With --apply: allow this newly advertised operation (PLUGIN/OPERATION). Repeatable.
    #[arg(long = "add", value_name = "PLUGIN/OPERATION")]
    pub add: Vec<String>,
    /// With --apply: remove configured operations which are no longer advertised.
    #[arg(long)]
    pub remove_missing: bool,
    /// With --apply in a noninteractive session: confirm the changes named by flags.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// Run a plugin command and convert the outcome to an exit code.
///
/// 0 succeeded (or nothing to do), 1 failed, 2 `sync` found differences it did not apply.
pub async fn run(
    command: &PluginsCommand,
    config: Option<PathBuf>,
    dotenv: HashMap<String, String>,
) -> ExitCode {
    let outcome = async {
        let session = Session::new(config, dotenv)?;

        match command {
            PluginsCommand::Install(args) => install::run(&session, args).await,
            PluginsCommand::Uninstall(args) => uninstall::run(&session, args),
            PluginsCommand::List(args) => inspect::list(&session, args),
            PluginsCommand::Inspect(args) => inspect::run(&session, args).await,
            PluginsCommand::Enable(args) => toggle::run(&session, args, true),
            PluginsCommand::Disable(args) => toggle::run(&session, args, false),
            PluginsCommand::Sync(args) => sync::run(&session, args).await,
        }
    }
    .await;

    match outcome {
        Ok(code) => ExitCode::from(code),
        Err(failure) => {
            eprintln!("remedia-server: {failure}");

            ExitCode::from(failure.code)
        }
    }
}

/// `PLUGIN/OPERATION`, split at the first slash (identifiers never contain one).
pub fn parse_operation(value: &str) -> Result<(String, String), Failure> {
    let valid = value.split_once('/').filter(|(plugin, operation)| {
        remedia_server_config::valid_identifier(plugin)
            && remedia_server_config::valid_identifier(operation)
    });

    valid
        .map(|(plugin, operation)| (plugin.to_owned(), operation.to_owned()))
        .ok_or_else(|| {
            Failure::new(format!(
                "`{value}` is not a PLUGIN/OPERATION pair (for example example.source/source.stream)"
            ))
        })
}

/// An allowlist entry as it is written to the configuration. `replaySafe` is always explicit.
pub fn operation_entry(plugin_id: &str, operation: &str, replay_safe: bool) -> Json {
    Json::object([
        ("pluginId", Json::str(plugin_id)),
        ("operation", Json::str(operation)),
        ("replaySafe", Json::Bool(replay_safe)),
    ])
}

/// The configured hosts as raw JSON, in file order.
pub fn raw_hosts(document: &str) -> Vec<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(document)
        .ok()
        .and_then(|value| {
            value
                .get("pluginHosts")
                .and_then(|hosts| hosts.as_array().cloned())
        })
        .unwrap_or_default()
}

pub fn host_ids(document: &str) -> Vec<String> {
    raw_hosts(document)
        .iter()
        .filter_map(|host| host.get("id").and_then(|id| id.as_str()).map(str::to_owned))
        .collect()
}

pub fn print_restart_notice() {
    println!("{RESTART_NOTICE}");
}

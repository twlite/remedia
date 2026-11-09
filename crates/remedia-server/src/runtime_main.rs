//! Native runtime entry point. This binary is intentionally separate from
//! the lightweight CLI: the OS only loads FFmpeg when this executable starts.
mod discovery;
mod environment;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use remedia_server::{ServerConfig, ServerHandle};

#[derive(Debug, Parser)]
#[command(name = "remedia-server-runtime", version, hide = true)]
struct RuntimeCli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long = "env-file", value_name = "PATH")]
    env_files: Vec<PathBuf>,
    #[arg(long = "optional-env-file", value_name = "PATH")]
    optional_env_files: Vec<PathBuf>,
    #[arg(long)]
    doctor: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(RuntimeCli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("remedia-server: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: RuntimeCli) -> Result<(), Box<dyn std::error::Error>> {
    let values = environment::load_files(&cli.env_files, &cli.optional_env_files)?;
    let config_path = cli
        .config
        .unwrap_or(std::env::current_dir()?.join("remedia.config.json"));
    if cli.doctor {
        return doctor(&config_path, &values).await;
    }
    let config = ServerConfig::from_file_with_env(&config_path, &values)?;
    let server = Arc::new(ServerHandle::start(config).await?);
    let instance = match discovery::register(Arc::clone(&server)).await {
        Ok(instance) => Some(instance),
        Err(error) => {
            eprintln!("remedia-server: local status discovery unavailable: {error}");
            None
        }
    };
    println!(
        "remedia-server listening on {} (WebSocket /v1/ws)",
        server.local_addr()
    );
    let signal = shutdown_signal().await;
    drop(instance);
    let shutdown = server.shutdown().await;
    signal?;
    shutdown?;
    Ok(())
}

async fn doctor(
    path: &Path,
    variables: &HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Remedia server {}", env!("CARGO_PKG_VERSION"));
    println!(
        "Platform: {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("Executable: {}", std::env::current_exe()?.display());
    println!("FFmpeg libraries loaded by the native runtime:");
    let expected = [63, 63, 61, 12, 7];
    let mut abi_ok = true;
    for ((name, packed), major) in remedia_core::ffmpeg_runtime_versions()
        .into_iter()
        .zip(expected)
    {
        let runtime_major = packed >> 16;
        let runtime_minor = (packed >> 8) & 0xff;
        let runtime_micro = packed & 0xff;
        let status = if runtime_major == major {
            "OK"
        } else {
            "INCOMPATIBLE"
        };
        println!(
            "  {name:<12} {runtime_major}.{runtime_minor}.{runtime_micro} (expected ABI {major}) {status}"
        );
        abi_ok &= runtime_major == major;
    }
    for key in ["FFMPEG_DIR", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"] {
        if std::env::var_os(key).is_some() {
            println!("{key}: set");
        }
    }
    println!(
        "Environment file variables available for config interpolation: {}",
        variables.len()
    );
    if !abi_ok {
        return Err("FFmpeg runtime ABI versions do not match the required FFmpeg 9 majors".into());
    }
    match ServerConfig::from_file_with_env(path, variables) {
        Ok(config) => {
            println!("Config: {} (valid)", path.display());
            println!("Local discovery: ~/.remedia/run (authenticated loopback)");
            report_plugin_hosts(&config).await;
            Ok(())
        }
        Err(error) => {
            eprintln!("Config: {} (invalid: {error})", path.display());
            Err("doctor found configuration errors".into())
        }
    }
}

/// Probe each configured plugin host. Unreachable hosts are reported, not fatal: a host may
/// legitimately start after this node, and its health is also available from `node.stats`.
async fn report_plugin_hosts(config: &ServerConfig) {
    let diagnostics = remedia_server::diagnose_plugin_hosts(config).await;

    if diagnostics.is_empty() {
        println!("Plugin hosts: none configured");

        return;
    }

    println!("Plugin hosts:");

    for host in diagnostics {
        println!(
            "  {:<24} {:<12} {} ({} operations advertised)",
            host.id, host.state, host.origin, host.advertised_operations
        );

        if let Some(detail) = host.detail {
            println!("    discovery failed: {detail}");
        }

        for missing in host.missing_operations {
            println!("    allowed operation not advertised: {missing}");
        }
    }
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

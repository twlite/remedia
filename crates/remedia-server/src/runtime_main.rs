//! Native runtime entry point. This binary is intentionally separate from
//! the lightweight CLI: the OS only loads FFmpeg when this executable starts.
mod discovery;
mod environment;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use remedia_server::{ConfigError, ServerConfig, ServerHandle};

#[derive(Debug, Parser)]
#[command(name = "remedia-server-runtime", version = concat!(env!("REMEDIA_BUILD_VERSION"), " (commit ", env!("REMEDIA_BUILD_COMMIT"), ")"), hide = true)]
struct RuntimeCli {
    #[arg(long, hide = true)]
    library_versions: bool,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long = "env-file", value_name = "PATH")]
    env_files: Vec<PathBuf>,
    #[arg(long = "optional-env-file", value_name = "PATH")]
    optional_env_files: Vec<PathBuf>,
    #[arg(long)]
    doctor: bool,
    #[arg(long)]
    shutdown_on_stdin_close: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match RuntimeCli::try_parse() {
        Ok(cli) => cli,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayVersion => {
            println!(
                "remedia-server-runtime {} (commit {})",
                env!("REMEDIA_BUILD_VERSION"),
                env!("REMEDIA_BUILD_COMMIT")
            );
            print_library_versions();
            return ExitCode::SUCCESS;
        }
        Err(error) => error.exit(),
    };

    if cli.doctor {
        return doctor(&cli).await;
    }

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("remedia-server: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: RuntimeCli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.library_versions {
        print_library_versions();
        return Ok(());
    }
    let values = environment::load_files(&cli.env_files, &cli.optional_env_files)?;
    let config_path = cli
        .config
        .unwrap_or(std::env::current_dir()?.join("remedia.config.json"));
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
    let signal = if cli.shutdown_on_stdin_close {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        // A dedicated thread avoids an uncancellable Tokio blocking stdin task keeping the
        // async runtime alive when shutdown is instead triggered by an OS signal.
        std::thread::spawn(move || {
            let result = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            let _ = sender.send(result.map(|_| ()));
        });
        tokio::select! {
            result = shutdown_signal() => result,
            result = receiver => result.map_err(std::io::Error::other)?,
        }
    } else {
        shutdown_signal().await
    };
    drop(instance);
    let shutdown = server.shutdown().await;
    signal?;
    shutdown?;
    Ok(())
}

fn print_library_versions() {
    for (name, packed) in remedia_core::ffmpeg_runtime_versions() {
        println!(
            "lib{name}: {}.{}.{}",
            packed >> 16,
            (packed >> 8) & 0xff,
            packed & 0xff
        );
    }
}

async fn doctor(cli: &RuntimeCli) -> ExitCode {
    println!(
        "Remedia server {} (commit {})",
        env!("REMEDIA_BUILD_VERSION"),
        env!("REMEDIA_BUILD_COMMIT")
    );
    println!("Runtime: PASS (native libraries loaded)");
    println!(
        "Platform: {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if let Ok(executable) = std::env::current_exe() {
        println!("Executable: {}", executable.display());
    }
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
    if !abi_ok {
        eprintln!(
            "FFmpeg: FAIL (loaded library ABI majors do not match FFmpeg 9). Select matching libraries through your OS loader search paths."
        );
    }

    let variables = environment::load_files(&cli.env_files, &cli.optional_env_files);
    match &variables {
        Ok(values) => println!(
            "Environment: PASS ({} file variables available for configuration)",
            values.len()
        ),
        Err(error) => eprintln!(
            "Environment: FAIL ({error}). Correct the --env-file path or contents; use --optional-env-file for files that may be absent."
        ),
    }

    let mut config_status = "SKIPPED";
    let mut plugin_status = "SKIPPED";
    let config_path = cli
        .config
        .clone()
        .map(Ok)
        .unwrap_or_else(|| std::env::current_dir().map(|dir| dir.join("remedia.config.json")));
    match (config_path, &variables) {
        (Err(error), _) => {
            config_status = "FAIL";
            eprintln!(
                "Configuration: FAIL (cannot resolve default configuration path: {error}). Pass --config PATH."
            );
        }
        (Ok(path), Ok(values)) => match ServerConfig::from_file_with_env(&path, values) {
            Ok(config) => {
                config_status = "PASS";
                println!("Configuration: PASS ({} is valid)", path.display());
                println!("Local discovery: ~/.remedia/run (authenticated loopback)");
                plugin_status = if report_plugin_hosts(&config).await {
                    "PASS"
                } else {
                    "WARN"
                };
            }
            Err(ConfigError::Io { source, .. })
                if cli.config.is_none() && source.kind() == std::io::ErrorKind::NotFound =>
            {
                println!(
                    "Configuration: SKIPPED (default {} is absent; not required for native checks). Use remedia --generate-config-file to create it before serving.",
                    path.display()
                );
            }
            Err(error) => {
                config_status = "FAIL";
                eprintln!("Configuration: FAIL ({error})");
                match error {
                    ConfigError::Io { source, .. }
                        if source.kind() == std::io::ErrorKind::NotFound =>
                    {
                        eprintln!(
                            "Create the requested file with remedia --generate-config-file \"{}\", or correct --config PATH.",
                            path.display()
                        )
                    }
                    ConfigError::MissingEnvironment { variable, .. } => eprintln!(
                        "Set {variable} in the process environment or a file supplied with --env-file."
                    ),
                    _ => eprintln!(
                        "Correct the configuration at {} using the error above, then run doctor again.",
                        path.display()
                    ),
                }
            }
        },
        (Ok(_), Err(_)) => println!("Configuration: SKIPPED (environment file loading failed)"),
    }

    println!(
        "Summary: runtime PASS; FFmpeg {}; environment {}; configuration {config_status}; plugin hosts {plugin_status}",
        if abi_ok { "PASS" } else { "FAIL" },
        if variables.is_ok() { "PASS" } else { "FAIL" }
    );

    if abi_ok && variables.is_ok() && config_status != "FAIL" {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Probe each configured plugin host. Unreachable hosts are reported, not fatal: a host may
/// legitimately start after this node, and its health is also available from `node.stats`.
async fn report_plugin_hosts(config: &ServerConfig) -> bool {
    let diagnostics = remedia_server::diagnose_plugin_hosts(config).await;

    if diagnostics.is_empty() {
        println!("Plugin hosts: none configured");

        return true;
    }

    println!("Plugin hosts:");
    let mut healthy = true;

    for host in diagnostics {
        healthy &= matches!(host.state, "healthy" | "disabled");
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

    healthy
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

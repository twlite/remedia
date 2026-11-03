use std::path::PathBuf;
use std::process::ExitCode;

use remedia_server::{ServerConfig, ServerHandle};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("remedia-server: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = parse_args()?;
    let config = ServerConfig::from_file(&config_path)?;
    let server = ServerHandle::start(config).await?;
    println!(
        "remedia-server listening on {} (WebSocket /v1/ws)",
        server.local_addr()
    );
    shutdown_signal().await?;
    server.shutdown().await?;
    Ok(())
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

fn parse_args() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        None => Ok(std::env::current_dir()?.join("remedia.config.json")),
        Some(argument) if argument == "--help" || argument == "-h" => {
            println!(
                "Usage: remedia-server [--config <path>]\n\nThe default config path is ./remedia.config.json."
            );
            std::process::exit(0);
        }
        Some(argument) if argument == "--config" => {
            let Some(path) = args.next() else {
                return Err("--config requires a path".into());
            };
            if args.next().is_some() {
                return Err("unexpected extra command line argument".into());
            }
            Ok(PathBuf::from(path))
        }
        Some(_) => Err("expected --config <path> or --help".into()),
    }
}

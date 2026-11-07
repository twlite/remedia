use std::path::PathBuf;
use std::process::{Command, ExitCode};
use clap::{Parser, Subcommand};
use remedia_server::{ServerConfig, ServerHandle};

#[derive(Debug, Parser)]
#[command(name = "remedia", version, about = "Remedia standalone audio server")]
struct Cli {
    /// Configuration file for the server (defaults to ./remedia.config.json).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the server (the default when no subcommand is supplied).
    Serve,
    /// Install the newest published server release and compatible FFmpeg runtime.
    #[command(alias = "upgrade")]
    Update {
        /// Keep the current FFmpeg installation.
        #[arg(long)]
        no_ffmpeg: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("remedia: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Some(Commands::Update { no_ffmpeg }) => update(no_ffmpeg),
        Some(Commands::Serve) | None => {
            let path = cli.config.unwrap_or(std::env::current_dir()?.join("remedia.config.json"));
            let config = ServerConfig::from_file(&path)?;
            let server = ServerHandle::start(config).await?;
            println!(
                "remedia-server listening on {} (WebSocket /v1/ws)",
                server.local_addr()
            );
            shutdown_signal().await?;
            server.shutdown().await?;
            Ok(())
        }
    }
}

fn update(no_ffmpeg: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Reuse the installer packaged with this exact server build. The script
    // selects a published GitHub release and verifies the asset SHA-256.
    let (source, extension) = if cfg!(windows) {
        (include_str!("../../../scripts/install.ps1"), "ps1")
    } else {
        (include_str!("../../../scripts/install.sh"), "sh")
    };
    let script_path = std::env::temp_dir().join(format!(
        "remedia-update-{}-{}.{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos(),
        extension
    ));
    std::fs::write(&script_path, source)?;
    let mut process = if cfg!(windows) {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
        command.arg(&script_path);
        if no_ffmpeg { command.arg("-NoFFmpeg"); }
        command
    } else {
        let mut command = Command::new("sh");
        command.arg(&script_path);
        if no_ffmpeg { command.arg("--no-ffmpeg"); }
        command
    };
    // Updating an installed Windows binary uses a new versioned directory,
    // so the running executable is not replaced in place.
    let status = process.status();
    let _ = std::fs::remove_file(script_path);
    if !status?.success() {
        return Err("the installer failed; the current installation was not switched".into());
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_serve_and_legacy_config_flags() {
        assert!(Cli::try_parse_from(["remedia"]).unwrap().command.is_none());
        let parsed = Cli::try_parse_from(["remedia", "--config", "test.json"]).unwrap();
        assert_eq!(parsed.config, Some(PathBuf::from("test.json")));
        assert!(matches!(Cli::try_parse_from(["remedia", "serve"]).unwrap().command, Some(Commands::Serve)));
    }
    #[test]
    fn supports_upgrade_alias() {
        assert!(matches!(
            Cli::try_parse_from(["remedia", "upgrade", "--no-ffmpeg"]).unwrap().command,
            Some(Commands::Update { no_ffmpeg: true })
        ));
    }
}

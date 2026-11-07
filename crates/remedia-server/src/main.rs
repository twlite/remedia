mod discovery;
mod environment;

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use remedia_server::{ServerConfig, ServerHandle};

#[derive(Debug, Parser)]
#[command(
    name = "remedia-server",
    version,
    about = "Remedia standalone Rust audio server",
    long_about = None
)]
struct Cli {
    /// Server configuration file (default: ./remedia.config.json).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Required .env file. Can be provided more than once.
    #[arg(long = "env-file", value_name = "PATH", global = true)]
    env_files: Vec<PathBuf>,
    /// Optional .env file. Missing files are ignored, malformed files are not.
    #[arg(long = "optional-env-file", value_name = "PATH", global = true)]
    optional_env_files: Vec<PathBuf>,
    /// Generate a secure starter configuration without overwriting existing files.
    #[arg(long, num_args = 0..=1, default_missing_value = "remedia.config.json", value_name = "PATH")]
    generate_config_file: Option<PathBuf>,
    /// List live Remedia servers owned by the current user.
    #[arg(long)]
    list: bool,
    /// Emit machine-readable JSON with --list (or the list subcommand).
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the server (the default when no subcommand is supplied).
    Serve,
    /// Validate the selected configuration and inspect local runtime requirements.
    Doctor,
    /// List running Remedia instances for the current operating-system user.
    List,
    /// Install the latest published server release and compatible FFmpeg runtime.
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
            eprintln!("remedia-server: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = cli.generate_config_file {
        if cli.command.is_some() || cli.list || cli.json {
            return Err("--generate-config-file cannot be combined with a command or --list/--json".into());
        }
        return generate_config_file(&path);
    }
    if cli.list || matches!(cli.command.as_ref(), Some(Commands::List)) {
        if cli.list && cli.command.is_some() {
            return Err("--list cannot be combined with a subcommand".into());
        }
        return discovery::print_instances(&discovery::list().await?, cli.json);
    }
    if cli.json {
        return Err("--json requires --list or the list subcommand".into());
    }
    match cli.command {
        Some(Commands::Update { no_ffmpeg }) => update(no_ffmpeg),
        Some(Commands::Doctor) => {
            let values = environment::load_files(&cli.env_files, &cli.optional_env_files)?;
            let path = default_config(cli.config)?;
            doctor(&path, &values)
        }
        Some(Commands::Serve) | None => {
            let values = environment::load_files(&cli.env_files, &cli.optional_env_files)?;
            let path = default_config(cli.config)?;
            let config = ServerConfig::from_file_with_env(&path, &values)?;
            let server = Arc::new(ServerHandle::start(config).await?);
            let instance = discovery::register(Arc::clone(&server)).await?;
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
        Some(Commands::List) => unreachable!("handled above"),
    }
}

fn default_config(path: Option<PathBuf>) -> Result<PathBuf, std::io::Error> {
    Ok(path.unwrap_or(std::env::current_dir()?.join("remedia.config.json")))
}

/// Generate a minimal configuration: loopback binding, no accessible media hosts
/// or file roots, and an environment-sourced credential. The user must supply
/// REMEDIA_RPC_PASSWORD before starting.
fn generate_config_file(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = serde_json::json!({
        "bind": {"host": "127.0.0.1", "port": 8787},
        "users": [{"username": "media-bot", "password": "env('REMEDIA_RPC_PASSWORD')"}],
        "media": {
            "allowedFileRoots": [],
            "allowedUrlHosts": [],
            "allowHttp": false,
            "allowHttps": true,
            "blockPrivateNetworks": true
        }
    });
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let mut data = serde_json::to_vec_pretty(&config)?;
    data.push(b'\n');
    file.write_all(&data)?;
    file.sync_all()?;
    println!("Generated {}", path.display());
    println!("Set REMEDIA_RPC_PASSWORD, then run: remedia-server --config {}", path.display());
    Ok(())
}

fn doctor(
    path: &Path,
    variables: &HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Remedia server {}", env!("CARGO_PKG_VERSION"));
    println!("Platform: {} / {}", std::env::consts::OS, std::env::consts::ARCH);
    println!("Executable: {}", std::env::current_exe()?.display());
    println!("FFmpeg libraries loaded by the current executable:");
    let expected = [63, 63, 61, 12, 7];
    let mut abi_ok = true;
    for ((name, packed), major) in remedia_core::ffmpeg_runtime_versions()
        .into_iter()
        .zip(expected)
    {
        let runtime_major = packed >> 16;
        let runtime_minor = (packed >> 8) & 0xff;
        let runtime_micro = packed & 0xff;
        let status = if runtime_major == major { "OK" } else { "INCOMPATIBLE" };
        println!(
            "  {name:<12} {runtime_major}.{runtime_minor}.{runtime_micro}              (expected ABI {major}) {status}"
        );
        abi_ok &= runtime_major == major;
    }
    for key in ["FFMPEG_DIR", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"] {
        if std::env::var_os(key).is_some() {
            println!("{key}: set");
        }
    }
    println!("Environment file variables available for config interpolation: {}", variables.len());
    if !abi_ok {
        return Err("FFmpeg runtime ABI versions do not match the required FFmpeg 9 majors".into());
    }
    match ServerConfig::from_file_with_env(path, variables) {
        Ok(_) => {
            println!("Config: {} (valid)", path.display());
            println!("Local discovery: ~/.remedia/run (authenticated loopback)");
            Ok(())
        }
        Err(error) => {
            eprintln!("Config: {} (invalid: {error})", path.display());
            Err("doctor found configuration errors".into())
        }
    }
}

fn update(no_ffmpeg: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Use the installer embedded at build time. The installer verifies the
    // published server archive SHA-256 and manages FFmpeg separately.
    let (source, extension) = if cfg!(windows) {
        (include_str!("../../../scripts/install.ps1"), "ps1")
    } else {
        (include_str!("../../../scripts/install.sh"), "sh")
    };
    let script_path = std::env::temp_dir().join(format!(
        "remedia-update-{}-{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
        extension
    ));
    fs::write(&script_path, source)?;
    let mut process = if cfg!(windows) {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
        command.arg(&script_path);
        if no_ffmpeg {
            command.arg("-NoFFmpeg");
        }
        command
    } else {
        let mut command = Command::new("sh");
        command.arg(&script_path);
        if no_ffmpeg {
            command.arg("--no-ffmpeg");
        }
        command
    };
    let status = process.status();
    let _ = fs::remove_file(script_path);
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
    fn cli_supports_legacy_and_new_commands() {
        let parsed = Cli::try_parse_from(["remedia-server", "--config", "test.json"]).unwrap();
        assert_eq!(parsed.config, Some(PathBuf::from("test.json")));
        assert!(parsed.command.is_none());
        assert!(matches!(
            Cli::try_parse_from(["remedia", "serve"]).unwrap().command,
            Some(Commands::Serve)
        ));
        assert!(matches!(
            Cli::try_parse_from(["remedia", "doctor"]).unwrap().command,
            Some(Commands::Doctor)
        ));
        assert!(Cli::try_parse_from(["remedia", "--list", "--json"]).unwrap().list);
        assert!(matches!(
            Cli::try_parse_from(["remedia", "upgrade", "--no-ffmpeg"])
                .unwrap()
                .command,
            Some(Commands::Update { no_ffmpeg: true })
        ));
    }

    #[test]
    fn cli_accepts_optional_config_path_and_env_file_flags() {
        let args = Cli::try_parse_from([
            "remedia-server",
            "--generate-config-file",
            "--env-file",
            ".env",
            "--optional-env-file",
            ".env.local",
        ])
        .unwrap();
        assert_eq!(args.generate_config_file, Some(PathBuf::from("remedia.config.json")));
        assert_eq!(args.env_files, vec![PathBuf::from(".env")]);
        assert_eq!(args.optional_env_files, vec![PathBuf::from(".env.local")]);
        let custom = Cli::try_parse_from(["remedia", "--generate-config-file", "custom.json"]).unwrap();
        assert_eq!(custom.generate_config_file, Some(PathBuf::from("custom.json")));
    }

    #[test]
    fn generated_config_is_secure_and_refuses_to_overwrite() {
        let path = std::env::temp_dir().join(format!(
            "remedia-test-config-{}-{}.json",
            std::process::id(),
            rand::random::<u64>()
        ));
        generate_config_file(&path).unwrap();
        assert!(generate_config_file(&path).is_err());
        let content: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(content["bind"]["host"], "127.0.0.1");
        assert_eq!(content["users"][0]["password"], "env('REMEDIA_RPC_PASSWORD')");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }
        fs::remove_file(path).unwrap();
    }
}

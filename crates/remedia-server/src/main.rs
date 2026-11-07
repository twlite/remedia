//! Lightweight Remedia command line. This binary must never depend on
//! remedia-core, remedia-discord, or remedia-server's native runtime code.
//! All FFmpeg dependencies live in the separate remedia-server-runtime process.
mod discovery_client;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

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
    /// Generate a safe starter configuration without overwriting existing files.
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
    /// Start the FFmpeg-enabled server (default).
    Serve,
    /// Check native FFmpeg libraries and validate configuration.
    Doctor,
    /// List live Remedia instances for this operating-system user.
    List,
    /// Install the latest published server and compatible FFmpeg runtime.
    #[command(alias = "upgrade")]
    Update {
        /// Keep the current FFmpeg installation.
        #[arg(long)]
        no_ffmpeg: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("remedia-server: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let uses_config = cli.generate_config_file.is_none()
        && !cli.list
        && matches!(
            cli.command.as_ref(),
            None | Some(Commands::Serve | Commands::Doctor)
        );
    if !uses_config && (!cli.env_files.is_empty() || !cli.optional_env_files.is_empty()) {
        return Err(
            "--env-file and --optional-env-file are only valid with serve or doctor".into(),
        );
    }
    if let Some(path) = &cli.generate_config_file {
        if cli.command.is_some() || cli.list || cli.json {
            return Err(
                "--generate-config-file cannot be combined with a command or --list/--json".into(),
            );
        }
        return generate_config_file(path);
    }
    if cli.list || matches!(cli.command.as_ref(), Some(Commands::List)) {
        if cli.list && cli.command.is_some() {
            return Err("--list cannot be combined with a subcommand".into());
        }
        return discovery_client::print_instances(&discovery_client::list().await?, cli.json);
    }
    if cli.json {
        return Err("--json requires --list or the list subcommand".into());
    }
    match cli.command.as_ref() {
        Some(Commands::Update { no_ffmpeg }) => update(*no_ffmpeg),
        Some(Commands::Doctor) => launch_runtime(&cli, true),
        Some(Commands::Serve) | None => launch_runtime(&cli, false),
        Some(Commands::List) => unreachable!("handled above"),
    }
}

fn native_executable() -> io::Result<PathBuf> {
    if let Some(override_path) = std::env::var_os("REMEDIA_SERVER_RUNTIME") {
        let path = PathBuf::from(override_path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "REMEDIA_SERVER_RUNTIME points to a missing file: {}",
                path.display()
            ),
        ));
    }
    let executable = std::env::current_exe()?;
    let parent = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot locate the CLI installation directory",
        )
    })?;
    let base = format!("remedia-server-runtime{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = vec![parent.join(base)];
    if let Some(name) = executable.file_name().and_then(|s| s.to_str())
        && let Some(target) = name.strip_prefix("remedia-server-")
    {
        candidates.push(parent.join(format!("remedia-server-runtime-{target}")));
    }
    candidates.into_iter().find(|path| path.is_file()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "native runtime binary is missing next to {}. Reinstall Remedia, or set REMEDIA_SERVER_RUNTIME",
                executable.display()
            ),
        )
    })
}

fn launch_runtime(cli: &Cli, doctor: bool) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = native_executable()?;
    let mut command = Command::new(runtime);
    if let Some(path) = &cli.config {
        command.arg("--config").arg(path);
    }
    for path in &cli.env_files {
        command.arg("--env-file").arg(path);
    }
    for path in &cli.optional_env_files {
        command.arg("--optional-env-file").arg(path);
    }
    if doctor {
        command.arg("--doctor");
        println!("Remedia CLI {}", env!("CARGO_PKG_VERSION"));
        println!(
            "Platform: {} / {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let output = command.output().map_err(|error| {
            format!("failed to start the native runtime: {error}. Install the required FFmpeg 9 shared libraries")
        })?;
        io::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        if output.status.success() {
            return Ok(());
        }
        return Err(
            "native FFmpeg runtime checks failed. Install FFmpeg 9 shared libraries (or reinstall with the Remedia installer) and retry"
                .into(),
        );
    }
    // On Unix, replacing our process ensures SIGTERM, signals, exit codes, and
    // service-manager PID tracking belong to the real runtime, not a wrapper.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(Box::new(error))
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("native Remedia runtime exited with {status}. Verify FFmpeg 9 shared libraries with remedia-server doctor").into())
        }
    }
}

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
    println!(
        "Set REMEDIA_RPC_PASSWORD, then run: remedia-server --config {}",
        path.display()
    );
    Ok(())
}

fn update(no_ffmpeg: bool) -> Result<(), Box<dyn std::error::Error>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_operations_do_not_construct_a_native_runtime() {
        assert!(Cli::try_parse_from(["remedia-server", "--help"]).is_err());
        assert!(Cli::try_parse_from(["remedia-server", "--version"]).is_err());
        assert!(
            Cli::try_parse_from(["remedia-server", "--list"])
                .unwrap()
                .list
        );
        assert!(
            Cli::try_parse_from(["remedia-server", "--generate-config-file"])
                .unwrap()
                .generate_config_file
                .is_some()
        );
        assert!(matches!(
            Cli::try_parse_from(["remedia-server", "doctor"])
                .unwrap()
                .command,
            Some(Commands::Doctor)
        ));
    }

    #[test]
    fn upgrade_alias_remains_supported() {
        assert!(matches!(
            Cli::try_parse_from(["remedia-server", "upgrade", "--no-ffmpeg"])
                .unwrap()
                .command,
            Some(Commands::Update { no_ffmpeg: true })
        ));
    }
}

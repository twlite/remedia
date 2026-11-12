//! Lightweight Remedia command line. This binary must never depend on
//! remedia-core, remedia-discord, or remedia-server's native runtime code.
//! All FFmpeg dependencies live in the separate remedia-server-runtime process.
mod discovery_client;
mod environment;
mod plugins_cli;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "remedia-server",
    version = env!("REMEDIA_BUILD_VERSION"),
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
    /// Shut down gracefully when the supervising process closes stdin (serve only).
    #[arg(long, global = true)]
    shutdown_on_stdin_close: bool,
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
    /// Register and manage remote plugin hosts in the configuration file.
    ///
    /// Registers independent @remedia/plugin-host deployments; it does not install plugin code.
    #[command(subcommand)]
    Plugins(plugins_cli::PluginsCommand),
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
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayVersion => {
            print_version();
            return ExitCode::SUCCESS;
        }
        Err(error) => error.exit(),
    };

    if let Some(Commands::Plugins(command)) = &cli.command {
        return run_plugins(&cli, command).await;
    }

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("remedia-server: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Plugin management only edits configuration: it never starts the runtime, so it keeps working
/// without FFmpeg. Global flags which make no sense for it are rejected rather than ignored.
async fn run_plugins(cli: &Cli, command: &plugins_cli::PluginsCommand) -> ExitCode {
    let rejected = if cli.shutdown_on_stdin_close {
        Some("--shutdown-on-stdin-close is only valid with serve")
    } else if cli.generate_config_file.is_some() || cli.list {
        Some("--generate-config-file and --list cannot be combined with a command")
    } else if cli.json {
        Some("--json belongs after the command, for example: plugins list --json")
    } else {
        None
    };

    if let Some(message) = rejected {
        eprintln!("remedia-server: {message}");

        return ExitCode::FAILURE;
    }

    match environment::load_files(&cli.env_files, &cli.optional_env_files) {
        Ok(dotenv) => plugins_cli::run(command, cli.config.clone(), dotenv).await,
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
    if cli.shutdown_on_stdin_close
        && (!uses_config || !matches!(cli.command.as_ref(), None | Some(Commands::Serve)))
    {
        return Err("--shutdown-on-stdin-close is only valid with serve".into());
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
        Some(Commands::Plugins(_)) => unreachable!("handled before run"),
    }
}

fn print_version() {
    println!(
        "remedia-server {} (commit {})",
        env!("REMEDIA_BUILD_VERSION"),
        env!("REMEDIA_BUILD_COMMIT")
    );

    // Probe in a separate process: a missing native library must never stop the CLI.
    let output = native_executable().ok().and_then(|runtime| {
        Command::new(runtime)
            .arg("--library-versions")
            .output()
            .ok()
    });
    match output {
        Some(output) if output.status.success() => {
            print!("{}", String::from_utf8_lossy(&output.stdout))
        }
        _ => println!("FFmpeg libraries: unavailable (native runtime could not be loaded)"),
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
    if cli.shutdown_on_stdin_close {
        command.arg("--shutdown-on-stdin-close");
    }
    if doctor {
        command.arg("--doctor");
        println!("Remedia CLI {}", env!("REMEDIA_BUILD_VERSION"));
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
        "$schema": "https://twlite.github.io/remedia/schemas/remedia.config.schema.json",
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
    // Updates target this installation even when the original installer used a custom prefix.
    if std::env::var_os("REMEDIA_INSTALL_DIR").is_none()
        && let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        let libexec = if parent.file_name().is_some_and(|name| name == "libexec") {
            Some(parent)
        } else {
            parent
                .parent()
                .filter(|path| path.file_name().is_some_and(|name| name == "libexec"))
        };
        if let Some(prefix) = libexec.and_then(Path::parent) {
            process.env("REMEDIA_INSTALL_DIR", prefix);
        }
    }
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

    #[tokio::test]
    async fn stdin_shutdown_is_only_valid_for_serve() {
        for args in [
            vec!["remedia-server", "--shutdown-on-stdin-close", "doctor"],
            vec!["remedia-server", "--shutdown-on-stdin-close", "--list"],
            vec![
                "remedia-server",
                "--shutdown-on-stdin-close",
                "--generate-config-file",
            ],
        ] {
            let error = run(Cli::try_parse_from(args).unwrap()).await.unwrap_err();
            assert!(error.to_string().contains("only valid with serve"));
        }
        assert!(
            Cli::try_parse_from(["remedia-server", "serve", "--shutdown-on-stdin-close"])
                .unwrap()
                .shutdown_on_stdin_close
        );
    }

    #[test]
    fn every_plugins_command_and_flag_parses() {
        use plugins_cli::PluginsCommand as P;

        let parse = |args: &[&str]| {
            let mut full = vec!["remedia-server"];

            full.extend_from_slice(args);

            Cli::try_parse_from(full).unwrap()
        };
        let plugins = |args: &[&str]| match parse(args).command {
            Some(Commands::Plugins(command)) => command,
            other => panic!("expected plugins, got {other:?}"),
        };

        let P::Install(install) = plugins(&[
            "plugins",
            "install",
            "https://p.example",
            "--id",
            "x",
            "--token-env",
            "T",
            "--operation",
            "a/b",
            "--operation",
            "c/d",
            "--all-operations",
            "--replay-safe",
            "a/b",
            "--allow-insecure-http",
            "--allow-private-network",
            "--yes",
            "--dry-run",
        ]) else {
            panic!("install")
        };

        assert_eq!(install.id.as_deref(), Some("x"));
        assert_eq!(install.token_env.as_deref(), Some("T"));
        assert_eq!(install.operations, ["a/b", "c/d"]);
        assert!(install.all_operations && install.yes && install.dry_run);
        assert!(install.allow_insecure_http && install.allow_private_network);
        assert!(
            matches!(plugins(&["plugins", "uninstall", "x", "--yes", "--dry-run"]), P::Uninstall(args) if args.yes && args.dry_run)
        );
        assert!(matches!(plugins(&["plugins", "list", "--json"]), P::List(args) if args.json));
        assert!(
            matches!(plugins(&["plugins", "inspect", "x", "--check", "--json"]), P::Inspect(args) if args.check && args.json)
        );
        assert!(
            matches!(plugins(&["plugins", "enable", "x", "--dry-run"]), P::Enable(args) if args.dry_run)
        );
        assert!(matches!(
            plugins(&["plugins", "disable", "x"]),
            P::Disable(_)
        ));
        assert!(
            matches!(plugins(&["plugins", "sync"]), P::Sync(args) if args.host_id.is_none() && !args.apply)
        );
        assert!(matches!(
            plugins(&["plugins", "sync", "x", "--dry-run", "--apply", "--add", "a/b", "--remove-missing", "--yes"]),
            P::Sync(args) if args.host_id.as_deref() == Some("x") && args.apply && args.remove_missing && args.add == ["a/b"]
        ));

        // The global options apply to plugin commands in any position.
        let cli = parse(&["--config", "c.json", "--env-file", "e", "plugins", "list"]);

        assert_eq!(cli.config.as_deref(), Some(Path::new("c.json")));
        assert!(Cli::try_parse_from(["remedia-server", "plugins", "install"]).is_err());
        assert!(Cli::try_parse_from(["remedia-server", "plugins", "frobnicate"]).is_err());
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

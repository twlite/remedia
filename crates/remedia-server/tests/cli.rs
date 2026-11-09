//! Black-box CLI smoke tests. No real Discord credentials or media sources.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn temp_root() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "remedia-cli-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn cli(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remedia-server"));
    command.env("HOME", home).env("USERPROFILE", home);
    command
}

#[test]
fn cli_commands_remain_available_without_ffmpeg_loader_paths() {
    let root = temp_root();
    let config = root.join("starter.json");
    let commands: Vec<Vec<&str>> = vec![
        vec!["--version"],
        vec!["--help"],
        vec!["update", "--help"],
        vec!["--list", "--json"],
    ];
    for args in commands {
        let output = cli(&root)
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("DYLD_LIBRARY_PATH")
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = cli(&root)
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("DYLD_LIBRARY_PATH")
        .args(["--generate-config-file", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(config.exists());
    let generated: serde_json::Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../packages/remedia-config/remedia.config.schema.json"
    ))
    .unwrap();
    assert_eq!(generated["$schema"], schema["$id"]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn generated_configuration_is_not_overwritten() {
    let root = temp_root();
    let path = root.join("config.json");
    assert!(
        cli(&root)
            .args(["--generate-config-file", path.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    let initial = fs::read(&path).unwrap();
    assert!(
        !cli(&root)
            .args(["--generate-config-file", path.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(fs::read(&path).unwrap(), initial);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn env_files_and_doctor_are_strict() {
    let root = temp_root();
    let config = root.join("config.json");
    let required = root.join(".env");
    let absent = root.join(".missing");
    fs::write(
        &config,
        r#"{"users":[{"username":"bot","password":"env('REMEDIA_TEST_CLI_PASSWORD')"}]}"#,
    )
    .unwrap();
    fs::write(&required, "REMEDIA_TEST_CLI_PASSWORD='test-secret'\n").unwrap();
    let passing = cli(&root)
        .args([
            "doctor",
            "--config",
            config.to_str().unwrap(),
            "--env-file",
            required.to_str().unwrap(),
            "--optional-env-file",
            absent.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        passing.status.success(),
        "{}",
        String::from_utf8_lossy(&passing.stderr)
    );

    assert!(
        !cli(&root)
            .args([
                "doctor",
                "--config",
                config.to_str().unwrap(),
                "--env-file",
                absent.to_str().unwrap()
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn list_reads_live_server_stats_without_rpc_credentials() {
    let root = temp_root();
    let config = root.join("config.json");
    let password = root.join(".env");
    fs::write(&config, r#"{"bind":{"host":"127.0.0.1","port":0},"users":[{"username":"bot","password":"env('REMEDIA_TEST_CLI_PASSWORD')"}]}"#).unwrap();
    fs::write(&password, "REMEDIA_TEST_CLI_PASSWORD=test-secret\n").unwrap();
    let mut server = cli(&root)
        .args([
            "serve",
            "--config",
            config.to_str().unwrap(),
            "--env-file",
            password.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(12);
    let mut found = None;
    while Instant::now() < deadline {
        if let Some(status) = server.try_wait().unwrap() {
            panic!("server exited unexpectedly: {status}");
        }
        let output = cli(&root).args(["--list", "--json"]).output().unwrap();
        if output.status.success()
            && let Ok(rows) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        {
            // Unix exec replaces the CLI process. Windows spawns a separate
            // runtime process, so its registered PID is different.
            found = rows.as_array().and_then(|rows| {
                rows.iter()
                    .find(|row| row["stats"]["nodeId"].is_string())
                    .cloned()
            });
            if found.is_some() {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    #[cfg(windows)]
    {
        // The Windows CLI waits for its child; killing only the CLI would
        // leave the native runtime running after the test ends.
        let _ = Command::new("taskkill")
            .args(["/PID", &server.id().to_string(), "/T", "/F"])
            .output();
    }
    #[cfg(not(windows))]
    let _ = server.kill();
    let _ = server.wait();
    let row = found.expect("expected live server discovery via loopback status endpoint");
    assert_eq!(row["stats"]["players"]["active"], 0);
    assert_eq!(row["stats"]["connections"]["active"], 0);
    assert!(row["address"].as_str().unwrap().contains(':'));
    assert!(row["stats"]["nodeId"].is_string());
    assert_eq!(row["stats"]["resources"]["samplingIntervalMs"], 1000);
    assert!(row["stats"]["memory"].get("privateResidentBytes").is_some());
    assert!(
        row["stats"]["memory"]
            .get("committedPrivateBytes")
            .is_some()
    );
    fs::remove_dir_all(root).unwrap();
}

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

#[test]
fn version_identity_survives_a_missing_native_runtime() {
    let root = temp_root();
    let output = cli(&root)
        .env("REMEDIA_SERVER_RUNTIME", root.join("missing-runtime"))
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with(&format!(
        "remedia-server {} (commit {})",
        env!("REMEDIA_BUILD_VERSION"),
        env!("REMEDIA_BUILD_COMMIT")
    )));
    assert!(stdout.contains("FFmpeg libraries: unavailable"));

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn update_reuses_installer_version_check_and_infers_custom_prefix() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_root();
    let libexec = root.join("libexec");
    let version_dir = libexec.join("fixture");
    let fake_bin = root.join("fake-bin");
    fs::create_dir_all(&version_dir).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();
    let executable = version_dir.join("remedia-server");
    fs::copy(env!("CARGO_BIN_EXE_remedia-server"), &executable).unwrap();
    fs::copy(&executable, version_dir.join("remedia-server-runtime")).unwrap();
    fs::write(libexec.join("current.txt"), "fixture").unwrap();

    // The installer publishes Linux/macOS server archives; Rust tests also run on
    // FreeBSD. Keep this installation fixture independent of the CI host's platform.
    let uname = fake_bin.join("uname");
    fs::write(
        &uname,
        "#!/bin/sh\ncase \"$1\" in -s) echo Linux ;; -m) echo x86_64 ;; *) exit 2 ;; esac\n",
    )
    .unwrap();
    fs::set_permissions(&uname, fs::Permissions::from_mode(0o755)).unwrap();

    let curl = fake_bin.join("curl");
    fs::write(&curl, format!(
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in -o) shift; out=\"$1\" ;; https:*) url=\"$1\" ;; esac; shift; done\ncase \"$url\" in */releases/latest) printf '{{\"tag_name\":\"v{}\"}}' > \"$out\"; printf 200 ;; *) exit 22 ;; esac\n",
        env!("REMEDIA_BUILD_VERSION")
    )).unwrap();
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();

    let mut update = Command::new(executable);
    update
        .args(["update", "--no-ffmpeg"])
        .env_remove("REMEDIA_INSTALL_DIR")
        .env_remove("REMEDIA_VERSION")
        .env(
            "PATH",
            format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap()),
        );
    let output = update.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("already current"));
    assert_eq!(
        fs::read_to_string(libexec.join("current.txt")).unwrap(),
        "fixture"
    );

    // Unsupported hosts must still fail before release lookup or installation changes.
    fs::write(
        &uname,
        "#!/bin/sh\ncase \"$1\" in -s) echo FreeBSD ;; -m) echo amd64 ;; *) exit 2 ;; esac\n",
    )
    .unwrap();
    let unsupported = update.output().unwrap();

    assert_eq!(unsupported.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&unsupported.stderr)
            .contains("Unsupported remedia-server platform: FreeBSD/amd64")
    );
    assert!(!String::from_utf8_lossy(&unsupported.stdout).contains("Checking GitHub releases"));
    assert_eq!(
        fs::read_to_string(libexec.join("current.txt")).unwrap(),
        "fixture"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn doctor_keeps_ffmpeg_success_separate_from_configuration_results() {
    let cases = [
        (None, false, 0, "SKIPPED", "default"),
        (None, true, 1, "FAIL", "Create the requested file"),
        (Some("{"), false, 1, "FAIL", "configuration JSON is invalid"),
        (Some("{"), true, 1, "FAIL", "configuration JSON is invalid"),
        (
            Some(
                r#"{"users":[{"username":"bot","password":"env('REMEDIA_DOCTOR_TEST_MISSING_SECRET')"}]}"#,
            ),
            true,
            1,
            "FAIL",
            "Set REMEDIA_DOCTOR_TEST_MISSING_SECRET",
        ),
        (
            Some(
                r#"{"users":[{"username":"bot","password":"test-only"}],"media":{"allowedFileRoots":["missing-media"]}}"#,
            ),
            true,
            1,
            "FAIL",
            "media.allowedFileRoots",
        ),
        (
            Some(r#"{"users":[{"username":"bot","password":"test-only"}]}"#),
            false,
            0,
            "PASS",
            "is valid",
        ),
    ];

    for (contents, explicit, code, config_status, detail) in cases {
        let root = temp_root();
        let path = root.join("remedia.config.json");
        if let Some(contents) = contents {
            fs::write(&path, contents).unwrap();
        }

        let mut command = cli(&root);
        command
            .current_dir(&root)
            .arg("doctor")
            .env_remove("REMEDIA_DOCTOR_TEST_MISSING_SECRET");
        if explicit {
            command.arg("--config").arg(&path);
        }
        let output = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let report = format!("{stdout}\n{stderr}");

        assert_eq!(output.status.code(), Some(code), "{report}");
        for major in [63, 63, 61, 12, 7] {
            assert!(
                stdout.contains(&format!("(expected ABI {major}) OK")),
                "{report}"
            );
        }
        assert!(stdout.contains(&format!("Summary: runtime PASS; FFmpeg PASS; environment PASS; configuration {config_status}")), "{report}");
        assert!(stdout.contains(&format!(
            "Remedia CLI {} (commit {})",
            env!("REMEDIA_BUILD_VERSION"),
            env!("REMEDIA_BUILD_COMMIT")
        )));
        assert!(stdout.contains(&format!(
            "Remedia server {} (commit {})",
            env!("REMEDIA_BUILD_VERSION"),
            env!("REMEDIA_BUILD_COMMIT")
        )));
        assert!(report.contains(detail), "{report}");
        assert!(!report.contains("Install FFmpeg"), "{report}");
        assert!(!report.contains("reinstall"), "{report}");
        assert!(
            !report.contains("native FFmpeg runtime checks failed"),
            "{report}"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn doctor_reports_environment_file_failures_without_hiding_ffmpeg_results() {
    let root = temp_root();
    let env_file = root.join(".env");

    for contents in [None, Some("not-an-assignment")] {
        if let Some(contents) = contents {
            fs::write(&env_file, contents).unwrap();
        }
        let output = cli(&root)
            .current_dir(&root)
            .arg("doctor")
            .arg("--env-file")
            .arg(&env_file)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(1));
        assert!(
            stdout.contains("FFmpeg PASS; environment FAIL; configuration SKIPPED"),
            "{stdout}"
        );
        assert!(stderr.contains("Environment: FAIL"), "{stderr}");
        assert!(stderr.contains("--optional-env-file"), "{stderr}");
        assert!(!stderr.contains("Install FFmpeg"), "{stderr}");
    }

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn doctor_preserves_unexpected_runtime_exit_codes_and_signals() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_root();
    let runtime = root.join("runtime");
    for (failure, code) in [("exit 37", 37), ("kill -TERM $$", 143)] {
        fs::write(
            &runtime,
            format!("#!/bin/sh\necho 'unrelated runtime failure' >&2\n{failure}\n"),
        )
        .unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        let output = cli(&root)
            .env("REMEDIA_SERVER_RUNTIME", &runtime)
            .arg("doctor")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(code));
        assert!(stdout.contains("runtime FAIL"), "{stdout}");
        assert!(stdout.contains("FFmpeg NOT CHECKED"), "{stdout}");
        assert!(stderr.contains("unrelated runtime failure"), "{stderr}");
        assert!(!stderr.contains("Install FFmpeg"), "{stderr}");
    }

    fs::remove_dir_all(root).unwrap();
}

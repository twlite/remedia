//! Black-box tests of `remedia-server plugins ...` against local mock plugin hosts.
//!
//! The commands only edit configuration and never start the native runtime, so these tests
//! also prove they work without FFmpeg: `REMEDIA_SERVER_RUNTIME` points at a missing file.
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

const TOKEN: &str = "cli-test-token-0123456789abcdef";

#[derive(Clone, Copy, PartialEq)]
enum Behaviour {
    Normal,
    RequireToken,
    Redirect,
    Malformed,
    OldProtocol,
    Hang,
    Missing,
}

struct Host {
    address: SocketAddr,
    behaviour: Arc<Mutex<Behaviour>>,
    manifest: Arc<Mutex<Value>>,
    requests: Arc<AtomicUsize>,
    authorizations: Arc<Mutex<Vec<String>>>,
    /// A file the mock edits while answering, simulating a concurrent writer.
    touch: Arc<Mutex<Option<PathBuf>>>,
}

fn manifest(name: Option<&str>, host_id: Option<&str>, operations: &[(&str, &str)]) -> Value {
    let mut plugins: Vec<Value> = Vec::new();

    for (plugin, operation) in operations {
        match plugins.iter_mut().find(|item| item["id"] == *plugin) {
            Some(item) => item["operations"]
                .as_array_mut()
                .unwrap()
                .push(json!(operation)),
            None => plugins.push(json!({"id": plugin, "operations": [operation]})),
        }
    }

    let mut value = json!({
        "protocolVersion": 1,
        "endpoints": {"execute": "/remedia/v1/execute", "media": "/remedia/v1/media/{token}"},
        "plugins": plugins,
    });

    if let Some(name) = name {
        value["name"] = json!(name);
    }

    if let Some(host_id) = host_id {
        value["hostId"] = json!(host_id);
    }

    value
}

impl Host {
    fn start(manifest: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = Self {
            address: listener.local_addr().unwrap(),
            behaviour: Arc::new(Mutex::new(Behaviour::Normal)),
            manifest: Arc::new(Mutex::new(manifest)),
            requests: Arc::new(AtomicUsize::new(0)),
            authorizations: Arc::new(Mutex::new(Vec::new())),
            touch: Arc::new(Mutex::new(None)),
        };
        let touch = Arc::clone(&host.touch);
        let (behaviour, manifest, requests, authorizations) = (
            Arc::clone(&host.behaviour),
            Arc::clone(&host.manifest),
            Arc::clone(&host.requests),
            Arc::clone(&host.authorizations),
        );

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buffer = [0_u8; 4096];
                let length = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..length]).to_string();

                requests.fetch_add(1, Ordering::SeqCst);

                if let Some(path) = touch.lock().unwrap().as_ref() {
                    let mut contents = fs::read_to_string(path).unwrap();

                    contents.push('\n');
                    fs::write(path, contents).unwrap();
                }

                let authorization = request
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .starts_with("authorization:")
                            .then(|| line[14..].trim().to_owned())
                    })
                    .unwrap_or_default();

                authorizations.lock().unwrap().push(authorization.clone());

                let mode = *behaviour.lock().unwrap();
                let body = manifest.lock().unwrap().to_string();
                let response = match mode {
                    Behaviour::Hang => {
                        thread::sleep(Duration::from_secs(30));
                        continue;
                    }
                    Behaviour::Missing => "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_owned(),
                    Behaviour::Redirect => "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:1/\r\ncontent-length: 0\r\n\r\n".to_owned(),
                    Behaviour::Malformed => ok("this is not json"),
                    Behaviour::OldProtocol => ok(&body.replace("\"protocolVersion\":1", "\"protocolVersion\":9")),
                    Behaviour::RequireToken if authorization != format!("Bearer {TOKEN}") => {
                        "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n".to_owned()
                    }
                    _ => ok(&body),
                };

                let _ = stream.write_all(response.as_bytes());
            }
        });

        host
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn set(&self, behaviour: Behaviour) {
        *self.behaviour.lock().unwrap() = behaviour;
    }
}

fn ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

struct Workspace {
    root: PathBuf,
    config: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "remedia-plugins-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));

        fs::create_dir_all(&root).unwrap();

        Self {
            config: root.join("remedia.config.json"),
            root,
        }
    }

    fn with_config(contents: &str) -> Self {
        let workspace = Self::new();

        fs::write(&workspace.config, contents).unwrap();

        workspace
    }

    fn read(&self) -> String {
        fs::read_to_string(&self.config).unwrap()
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.read()).unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, "", &[])
    }

    fn run_with(&self, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remedia-server"));

        command
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env(
                "REMEDIA_SERVER_RUNTIME",
                self.root.join("no-native-runtime"),
            )
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("DYLD_LIBRARY_PATH")
            .env_remove("REMEDIA_FORCE_INTERACTIVE")
            .env_remove("CLI_TEST_TOKEN")
            .envs(env.iter().copied())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().unwrap();

        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

const BASE: &str = "{\n  \"$schema\": \"https://example.test/schema.json\",\n  \"users\": [{ \"username\": \"bot\", \"password\": \"env('RPC_PASSWORD')\" }]\n}\n";

fn standard_host() -> Host {
    Host::start(manifest(
        Some("Example Host"),
        None,
        &[
            ("example.source", "source.stream"),
            ("example.metadata", "catalog.search"),
            ("example.metadata", "lyrics.search"),
        ],
    ))
}

fn install_args<'a>(host: &'a Host, extra: &[&'a str]) -> Vec<String> {
    let mut args: Vec<String> = ["plugins", "install", &host.url()]
        .map(String::from)
        .to_vec();

    args.extend(extra.iter().map(|value| (*value).to_owned()));

    args
}

fn run_install(workspace: &Workspace, host: &Host, extra: &[&str]) -> Output {
    let args = install_args(host, extra);

    workspace.run(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

#[test]
fn install_registers_only_the_selected_operations_with_conservative_defaults() {
    let host = standard_host();
    let workspace = Workspace::with_config(BASE);
    let output = run_install(
        &workspace,
        &host,
        &[
            "--operation",
            "example.metadata/catalog.search",
            "--operation",
            "example.source/source.stream",
            "--yes",
        ],
    );

    assert_eq!(code(&output), 0, "{}", text(&output));
    assert!(text(&output).contains("Restart remedia-server"));

    let config = workspace.json();
    let entry = &config["pluginHosts"][0];

    // The display name became the stable ID; nothing was enabled implicitly.
    assert_eq!(entry["id"], "example-host");
    assert_eq!(entry["url"], host.url());
    assert_eq!(entry["allowHttp"], true);
    assert!(entry.get("auth").is_none());
    assert_eq!(
        entry["operations"],
        json!([
            "example.metadata/catalog.search",
            "example.source/source.stream",
        ])
    );
    // Unrelated content is untouched, including `$schema` and formatting.
    assert!(workspace.read().starts_with(&BASE[..BASE.len() - 3]));
    assert!(
        workspace
            .read()
            .contains("\"$schema\": \"https://example.test/schema.json\"")
    );
    assert!(workspace.read().ends_with("}\n"));
}

#[test]
fn replay_safety_is_an_explicit_operator_decision() {
    let host = standard_host();
    let workspace = Workspace::with_config(BASE);
    let output = run_install(
        &workspace,
        &host,
        &[
            "--id",
            "metadata",
            "--all-operations",
            "--replay-safe",
            "example.metadata/catalog.search",
            "--yes",
        ],
    );

    assert_eq!(code(&output), 0, "{}", text(&output));

    let operations = workspace.json()["pluginHosts"][0]["operations"].clone();

    for operation in operations.as_array().unwrap() {
        assert_eq!(
            operation
                .get("replaySafe")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            operation.get("operation").and_then(Value::as_str) == Some("catalog.search"),
            "{operation}"
        );
    }

    let other = run_install(
        &Workspace::with_config(BASE),
        &host,
        &[
            "--operation",
            "example.metadata/catalog.search",
            "--replay-safe",
            "example.source/source.stream",
            "--yes",
        ],
    );

    assert_eq!(code(&other), 1);
    assert!(text(&other).contains("not one of the selected operations"));
}

#[test]
fn noninteractive_installs_require_explicit_operations_and_confirmation() {
    let host = standard_host();
    let workspace = Workspace::with_config(BASE);
    let no_selection = run_install(&workspace, &host, &["--yes"]);

    assert_eq!(code(&no_selection), 1);
    assert!(text(&no_selection).contains("--operation"));

    let no_confirmation = run_install(&workspace, &host, &["--all-operations"]);

    assert_eq!(code(&no_confirmation), 1);
    assert!(text(&no_confirmation).contains("--yes"));
    assert_eq!(workspace.read(), BASE);

    let unadvertised = run_install(
        &workspace,
        &host,
        &["--operation", "example.metadata/not.advertised", "--yes"],
    );

    assert_eq!(code(&unadvertised), 1);
    assert!(text(&unadvertised).contains("does not advertise"));
    assert_eq!(workspace.read(), BASE);
}

#[test]
fn interactive_installs_prompt_for_operations_and_confirmation() {
    let host = standard_host();
    let workspace = Workspace::with_config(BASE);
    let args = install_args(&host, &[]);
    let output = workspace.run_with(
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
        "1,3\ny\n",
        &[("REMEDIA_FORCE_INTERACTIVE", "1")],
    );

    assert_eq!(code(&output), 0, "{}", text(&output));

    let operations = workspace.json()["pluginHosts"][0]["operations"].clone();

    assert_eq!(operations.as_array().unwrap().len(), 2);

    let declined = Workspace::with_config(BASE);
    let output = declined.run_with(
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
        "all\nn\n",
        &[("REMEDIA_FORCE_INTERACTIVE", "1")],
    );

    assert_eq!(code(&output), 1);
    assert_eq!(declined.read(), BASE);
}

#[test]
fn dry_run_changes_nothing_but_validates_the_result() {
    let host = standard_host();
    let workspace = Workspace::with_config(BASE);
    let output = run_install(&workspace, &host, &["--all-operations", "--dry-run"]);

    assert_eq!(code(&output), 0, "{}", text(&output));
    assert!(text(&output).contains("Dry run"));
    assert_eq!(workspace.read(), BASE);
    assert_eq!(fs::read_dir(&workspace.root).unwrap().count(), 1);
}

#[test]
fn a_declared_host_id_wins_and_conflicts_are_detected() {
    let host = Host::start(manifest(Some("Named"), Some("declared-id"), &[("p", "o")]));
    let workspace = Workspace::with_config(BASE);
    let mismatch = run_install(
        &workspace,
        &host,
        &["--id", "other", "--all-operations", "--yes"],
    );

    assert_eq!(code(&mismatch), 1);
    assert!(text(&mismatch).contains("declared-id"));

    let installed = run_install(&workspace, &host, &["--all-operations", "--yes"]);

    assert_eq!(code(&installed), 0, "{}", text(&installed));
    assert_eq!(workspace.json()["pluginHosts"][0]["id"], "declared-id");

    // The same host again, and the same ID on another url, are both refused.
    let again = run_install(&workspace, &host, &["--all-operations", "--yes"]);

    assert_eq!(code(&again), 1);
    assert!(text(&again).contains("already registered"));

    let other_host = Host::start(manifest(Some("Other"), Some("declared-id"), &[("p", "o")]));
    let clash = run_install(&workspace, &other_host, &["--all-operations", "--yes"]);

    assert_eq!(code(&clash), 1);
    assert!(text(&clash).contains("different host"));

    let same_url = Host::start(manifest(None, None, &[("p", "o")]));
    let first = run_install(
        &workspace,
        &same_url,
        &["--id", "one", "--all-operations", "--yes"],
    );
    let second = run_install(
        &workspace,
        &same_url,
        &["--id", "two", "--all-operations", "--yes"],
    );

    assert_eq!(code(&first), 0, "{}", text(&first));
    assert_eq!(code(&second), 1);
    assert!(text(&second).contains("already registered as `one`"));
    assert_eq!(workspace.json()["pluginHosts"].as_array().unwrap().len(), 2);
}

#[test]
fn authenticated_hosts_use_environment_references_and_never_leak_the_token() {
    let host = standard_host();

    host.set(Behaviour::RequireToken);

    let workspace = Workspace::with_config(BASE);
    let without = run_install(&workspace, &host, &["--all-operations", "--yes"]);

    assert_eq!(code(&without), 1);
    assert!(text(&without).contains("--token-env"));

    let unset = run_install(
        &workspace,
        &host,
        &["--token-env", "CLI_TEST_TOKEN", "--all-operations", "--yes"],
    );

    assert_eq!(code(&unset), 1);
    assert!(text(&unset).contains("CLI_TEST_TOKEN"));
    assert!(text(&unset).contains("not set"));

    let args = install_args(
        &host,
        &["--token-env", "CLI_TEST_TOKEN", "--all-operations", "--yes"],
    );
    let installed = workspace.run_with(
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
        "",
        &[("CLI_TEST_TOKEN", TOKEN)],
    );

    assert_eq!(code(&installed), 0, "{}", text(&installed));
    assert!(!text(&installed).contains(TOKEN));
    assert!(!workspace.read().contains(TOKEN));
    assert_eq!(
        workspace.json()["pluginHosts"][0]["auth"],
        json!({"type": "bearer", "token": "env('CLI_TEST_TOKEN')"})
    );
    assert!(
        host.authorizations
            .lock()
            .unwrap()
            .iter()
            .any(|value| value == &format!("Bearer {TOKEN}"))
    );

    // A dotenv file supplies the variable with the same precedence rules as `serve`.
    let dotenv = workspace.root.join("plugins.env");

    fs::write(&dotenv, format!("CLI_TEST_TOKEN={TOKEN}\n")).unwrap();

    let inspected = workspace.run(&[
        "--env-file",
        dotenv.to_str().unwrap(),
        "plugins",
        "inspect",
        "example-host",
        "--check",
        "--json",
    ]);

    assert_eq!(code(&inspected), 0, "{}", text(&inspected));
    assert!(!text(&inspected).contains(TOKEN));
    assert!(text(&inspected).contains("\"tokenEnv\": \"CLI_TEST_TOKEN\""));

    // A wrong token is reported as an authentication failure, still without echoing it.
    let wrong = workspace.run_with(
        &["plugins", "inspect", "example-host", "--check"],
        "",
        &[("CLI_TEST_TOKEN", "wrong-token-0123456789abcdef")],
    );

    assert_eq!(code(&wrong), 2);
    assert!(text(&wrong).contains("authentication_rejected"));
    assert!(!text(&wrong).contains("wrong-token"));
}

#[test]
fn unsafe_urls_and_hostile_hosts_are_rejected_and_yes_does_not_relax_them() {
    let host = standard_host();
    let workspace = Workspace::with_config(BASE);

    // Credentials, paths and queries are never allowed in configured origins.
    for args in [
        vec![
            "plugins",
            "install",
            "https://user:pw@plugins.example.com",
            "--all-operations",
            "--yes",
        ],
        vec![
            "plugins",
            "install",
            "https://plugins.example.com/path",
            "--all-operations",
            "--yes",
        ],
        vec![
            "plugins",
            "install",
            "https://plugins.example.com/?q=1",
            "--all-operations",
            "--yes",
        ],
        vec![
            "plugins",
            "install",
            "ftp://plugins.example.com",
            "--all-operations",
            "--yes",
        ],
        vec![
            "plugins",
            "install",
            "plugins.example.com",
            "--all-operations",
            "--yes",
        ],
    ] {
        let output = workspace.run(&args);

        assert_eq!(code(&output), 1, "{args:?}: {}", text(&output));
    }

    assert_eq!(
        host.requests.load(Ordering::SeqCst),
        0,
        "no unsafe request may be sent"
    );
    assert_eq!(workspace.read(), BASE);

    for (behaviour, expected) in [
        (Behaviour::Redirect, "redirect"),
        (Behaviour::Malformed, "not a Remedia manifest"),
        (Behaviour::OldProtocol, "unsupported protocol version"),
        (Behaviour::Missing, "does not implement the Remedia v1"),
    ] {
        host.set(behaviour);

        let output = run_install(&workspace, &host, &["--all-operations", "--yes"]);

        assert_eq!(code(&output), 1);
        assert!(text(&output).contains(expected), "{}", text(&output));
    }

    assert_eq!(workspace.read(), BASE);
}

#[test]
fn network_failures_and_timeouts_are_reported_without_changes() {
    let workspace = Workspace::with_config(BASE);
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", dead.local_addr().unwrap());

    drop(dead);

    let refused = workspace.run(&["plugins", "install", &url, "--all-operations", "--yes"]);

    assert_eq!(code(&refused), 1);
    assert!(text(&refused).contains("refused the connection"));
    assert_eq!(workspace.read(), BASE);
}

#[test]
fn missing_and_invalid_configurations_are_never_created_or_repaired() {
    let host = standard_host();
    let missing = Workspace::new();
    let output = run_install(&missing, &host, &["--all-operations", "--yes"]);

    assert_eq!(code(&output), 1);
    assert!(text(&output).contains("--generate-config-file"));
    assert!(!missing.config.exists());
    assert_eq!(host.requests.load(Ordering::SeqCst), 0);

    for broken in [
        "{ not json",
        "{\"users\":[]}",
        "{\"users\":[{\"username\":\"a\",\"password\":\"b\"}],\"unknown\":1}",
    ] {
        let workspace = Workspace::with_config(broken);

        for args in [
            vec!["plugins", "list"],
            vec!["plugins", "uninstall", "x", "--yes"],
            vec!["plugins", "disable", "x"],
            vec!["plugins", "sync"],
        ] {
            let output = workspace.run(&args);

            assert_eq!(code(&output), 1, "{args:?}");
            assert!(
                text(&output).contains("not a valid configuration"),
                "{}",
                text(&output)
            );
        }

        assert_eq!(workspace.read(), broken);
    }
}

fn with_hosts(host: &Host, extra: &str) -> String {
    format!(
        "{{\n  \"users\": [{{ \"username\": \"bot\", \"password\": \"x\" }}],\n  \"pluginHosts\": [\n    {{\n      \"id\": \"alpha\",\n      \"url\": \"{url}\",\n      \"allowHttp\": true,\n      \n      {extra}\n      \"operations\": [\n        {{ \"pluginId\": \"example.metadata\", \"operation\": \"catalog.search\", \"replaySafe\": true }},\n        {{ \"pluginId\": \"example.metadata\", \"operation\": \"retired.operation\" }}\n      ]\n    }},\n    {{\n      \"id\": \"beta\",\n      \"url\": \"https://beta.example.com\",\n      \"operations\": [{{ \"pluginId\": \"p\", \"operation\": \"o\" }}]\n    }}\n  ]\n}}\n",
        url = host.url()
    )
}

#[test]
fn list_works_offline_and_json_is_stable_and_credential_free() {
    let host = standard_host();
    let workspace = Workspace::with_config(&with_hosts(
        &host,
        "\"auth\": { \"type\": \"bearer\", \"token\": \"inline-token-0123456789abcdef\" },",
    ));
    let before = host.requests.load(Ordering::SeqCst);
    let table = workspace.run(&["plugins", "list"]);

    assert_eq!(code(&table), 0, "{}", text(&table));
    assert!(text(&table).contains("alpha"));
    assert!(text(&table).contains("configuration only"));
    assert!(!text(&table).contains("inline-token"));

    let json = workspace.run(&["plugins", "list", "--json"]);
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();

    assert_eq!(value["hosts"][0]["id"], "alpha");
    assert_eq!(value["hosts"][0]["enabled"], true);
    assert_eq!(value["hosts"][0]["operations"], 2);
    assert_eq!(
        value["hosts"][0]["auth"],
        json!({"type": "bearer", "tokenSource": "inline"})
    );
    assert_eq!(value["hosts"][0]["health"], "unverified");
    assert!(!text(&json).contains("inline-token"));
    // Listing is offline: not a single request reached any host.
    assert_eq!(host.requests.load(Ordering::SeqCst), before);

    let empty = Workspace::with_config(BASE);

    assert!(text(&empty.run(&["plugins", "list"])).contains("No plugin hosts"));
}

#[test]
fn inspect_is_offline_until_checked_and_reports_missing_operations() {
    let host = standard_host();
    let workspace = Workspace::with_config(&with_hosts(&host, ""));
    let offline = workspace.run(&["plugins", "inspect", "alpha"]);

    assert_eq!(code(&offline), 0, "{}", text(&offline));
    assert!(text(&offline).contains("retired.operation"));
    assert!(text(&offline).contains("Configuration only"));
    assert_eq!(host.requests.load(Ordering::SeqCst), 0);

    let checked = workspace.run(&["plugins", "inspect", "alpha", "--check", "--json"]);
    let value: Value = serde_json::from_slice(&checked.stdout).unwrap();

    assert_eq!(code(&checked), 2);
    assert_eq!(value["check"]["status"], "operations_missing");
    assert_eq!(
        value["check"]["missingOperations"],
        json!(["example.metadata/retired.operation"])
    );
    assert_eq!(value["check"]["protocolCompatible"], true);
    assert_eq!(value["config"]["operations"][0]["replaySafe"], true);

    host.set(Behaviour::OldProtocol);

    let incompatible = workspace.run(&["plugins", "inspect", "alpha", "--check"]);

    assert_eq!(code(&incompatible), 2);
    assert!(text(&incompatible).contains("incompatible"));

    let unknown = workspace.run(&["plugins", "inspect", "ghost"]);

    assert_eq!(code(&unknown), 1);
    assert!(text(&unknown).contains("alpha, beta"));
}

#[test]
fn uninstall_removes_one_host_only_and_repeating_it_is_an_error() {
    let host = standard_host();
    let workspace = Workspace::with_config(&with_hosts(&host, ""));
    let needs_confirmation = workspace.run(&["plugins", "uninstall", "alpha"]);

    assert_eq!(code(&needs_confirmation), 1);
    assert!(text(&needs_confirmation).contains("--yes"));

    let dry = workspace.run(&["plugins", "uninstall", "alpha", "--dry-run"]);

    assert_eq!(code(&dry), 0);
    assert!(text(&dry).contains("not contacted"));
    assert!(workspace.read().contains("\"alpha\""));

    let removed = workspace.run(&["plugins", "uninstall", "alpha", "--yes"]);

    assert_eq!(code(&removed), 0, "{}", text(&removed));
    assert!(text(&removed).contains("Restart remedia-server"));
    assert_eq!(workspace.json()["pluginHosts"].as_array().unwrap().len(), 1);
    assert!(workspace.read().contains("\"beta\""));
    assert!(workspace.read().contains("\"users\""));
    assert_eq!(
        host.requests.load(Ordering::SeqCst),
        0,
        "the remote service is never contacted"
    );

    let repeated = workspace.run(&["plugins", "uninstall", "alpha", "--yes"]);

    assert_eq!(code(&repeated), 1);
    assert!(text(&repeated).contains("no plugin host `alpha`"));

    let interactive = workspace.run_with(
        &["plugins", "uninstall", "beta"],
        "y\n",
        &[("REMEDIA_FORCE_INTERACTIVE", "1")],
    );

    assert_eq!(code(&interactive), 0, "{}", text(&interactive));
    assert!(workspace.read().contains("\"pluginHosts\": []"));
}

#[test]
fn disabling_and_enabling_persist_and_keep_the_registration() {
    let host = standard_host();
    let workspace = Workspace::with_config(&with_hosts(&host, ""));
    let disabled = workspace.run(&["plugins", "disable", "alpha"]);

    assert_eq!(code(&disabled), 0, "{}", text(&disabled));
    assert_eq!(workspace.json()["pluginHosts"][0]["enabled"], false);
    assert_eq!(
        workspace.json()["pluginHosts"][0]["operations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(text(&disabled).contains("Restart remedia-server"));

    let listed: Value =
        serde_json::from_slice(&workspace.run(&["plugins", "list", "--json"]).stdout).unwrap();

    assert_eq!(listed["hosts"][0]["enabled"], false);
    assert_eq!(listed["hosts"][1]["enabled"], true);
    assert!(text(&workspace.run(&["plugins", "disable", "alpha"])).contains("already disabled"));

    let after_disable = workspace.read();
    let dry = workspace.run(&["plugins", "enable", "alpha", "--dry-run"]);

    assert_eq!(code(&dry), 0);
    assert_eq!(workspace.read(), after_disable);

    let enabled = workspace.run(&["plugins", "enable", "alpha"]);

    assert_eq!(code(&enabled), 0, "{}", text(&enabled));
    assert_eq!(workspace.json()["pluginHosts"][0]["enabled"], true);
    assert_eq!(code(&workspace.run(&["plugins", "enable", "missing"])), 1);
}

#[test]
fn sync_reports_differences_read_only_by_default() {
    let host = standard_host();
    let workspace = Workspace::with_config(&with_hosts(&host, ""));
    let before = workspace.read();
    let output = workspace.run(&["plugins", "sync", "alpha"]);

    assert_eq!(code(&output), 2, "{}", text(&output));
    assert!(text(&output).contains("- example.metadata/retired.operation"));
    assert!(text(&output).contains("+ example.metadata/lyrics.search"));
    assert!(text(&output).contains("+ example.source/source.stream"));
    assert!(text(&output).contains("Nothing was changed"));
    assert_eq!(workspace.read(), before);

    // An unreachable or incompatible host is reported and its configuration kept.
    host.set(Behaviour::OldProtocol);

    let incompatible = workspace.run(&["plugins", "sync"]);

    assert_eq!(code(&incompatible), 2);
    assert!(text(&incompatible).contains("incompatible"));
    assert_eq!(workspace.read(), before);

    let in_sync = Host::start(manifest(
        None,
        None,
        &[("example.metadata", "catalog.search")],
    ));
    let clean = Workspace::with_config(&format!(
        "{{\"users\":[{{\"username\":\"b\",\"password\":\"x\"}}],\"pluginHosts\":[{{\"id\":\"c\",\"url\":\"{}\",\"allowHttp\":true,\"operations\":[{{\"pluginId\":\"example.metadata\",\"operation\":\"catalog.search\"}}]}}]}}",
        in_sync.url()
    ));

    assert_eq!(code(&clean.run(&["plugins", "sync"])), 0);
}

#[test]
fn sync_applies_only_approved_changes_and_never_enables_or_changes_anything_else() {
    let host = standard_host();
    let workspace = Workspace::with_config(&with_hosts(&host, ""));
    let unattended = workspace.run(&["plugins", "sync", "alpha", "--apply"]);

    assert_eq!(code(&unattended), 1);
    assert!(text(&unattended).contains("explicit changes"));

    let mismatched = workspace.run(&[
        "plugins",
        "sync",
        "alpha",
        "--apply",
        "--add",
        "example.metadata/not.advertised",
        "--yes",
    ]);

    assert_eq!(code(&mismatched), 1);
    assert!(text(&mismatched).contains("not a newly advertised"));

    let dry = workspace.run(&[
        "plugins",
        "sync",
        "alpha",
        "--apply",
        "--dry-run",
        "--add",
        "example.metadata/lyrics.search",
        "--remove-missing",
        "--yes",
    ]);

    assert_eq!(code(&dry), 0, "{}", text(&dry));
    assert!(text(&dry).contains("Dry run"));

    let applied = workspace.run(&[
        "plugins",
        "sync",
        "alpha",
        "--apply",
        "--add",
        "example.metadata/lyrics.search",
        "--remove-missing",
        "--yes",
    ]);

    assert_eq!(code(&applied), 0, "{}", text(&applied));
    assert!(text(&applied).contains("Restart remedia-server"));

    let operations = workspace.json()["pluginHosts"][0]["operations"].clone();
    let names: Vec<_> = operations
        .as_array()
        .unwrap()
        .iter()
        .map(|operation| {
            operation
                .as_str()
                .and_then(|name| name.split_once('/').map(|(_, op)| op))
                .unwrap_or_else(|| operation["operation"].as_str().unwrap())
        })
        .collect();

    // The retired operation is gone, one operation was added (replay-safe off), `source.stream`
    // was not enabled, and an existing replaySafe value is untouched.
    assert_eq!(names, ["catalog.search", "lyrics.search"]);
    assert_eq!(operations[0]["replaySafe"], true);
    assert_eq!(operations[1], "example.metadata/lyrics.search");
    assert_eq!(workspace.json()["pluginHosts"][1]["id"], "beta");
}

#[test]
fn sync_prompts_for_each_change_and_ignores_declined_ones() {
    let host = standard_host();
    let workspace = Workspace::with_config(&with_hosts(&host, ""));
    let output = workspace.run_with(
        &["plugins", "sync", "alpha", "--apply"],
        "n\ny\nn\ny\n",
        &[("REMEDIA_FORCE_INTERACTIVE", "1")],
    );

    assert_eq!(code(&output), 0, "{}", text(&output));

    let operations: Vec<String> = workspace.json()["pluginHosts"][0]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|operation| {
            operation
                .as_str()
                .and_then(|name| name.split_once('/').map(|(_, op)| op))
                .unwrap_or_else(|| operation["operation"].as_str().unwrap())
                .to_owned()
        })
        .collect();

    // retired.operation kept (n), lyrics.search added (y), source.stream skipped (n).
    assert_eq!(
        operations,
        ["catalog.search", "retired.operation", "lyrics.search"]
    );
}

#[test]
fn an_unreachable_host_never_loses_its_configuration() {
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", dead.local_addr().unwrap());

    drop(dead);

    let config = format!(
        "{{\"users\":[{{\"username\":\"b\",\"password\":\"x\"}}],\"pluginHosts\":[{{\"id\":\"gone\",\"url\":\"{url}\",\"allowHttp\":true,\"operations\":[{{\"pluginId\":\"p\",\"operation\":\"o\"}}]}}]}}"
    );
    let workspace = Workspace::with_config(&config);
    let output = workspace.run(&["plugins", "sync", "--apply", "--remove-missing", "--yes"]);

    assert_eq!(
        text(&output)
            .matches("Configuration left unchanged")
            .count(),
        1,
        "{}",
        text(&output)
    );
    assert_eq!(workspace.read(), config);
}

#[test]
fn custom_config_paths_and_global_flags_are_honored() {
    let host = standard_host();
    let workspace = Workspace::new();
    let custom = workspace.root.join("nested").join("server.json");

    fs::create_dir_all(custom.parent().unwrap()).unwrap();
    fs::write(&custom, BASE).unwrap();

    let args = install_args(
        &host,
        &[
            "--config",
            custom.to_str().unwrap(),
            "--all-operations",
            "--yes",
        ],
    );
    let output = workspace.run(&args.iter().map(String::as_str).collect::<Vec<_>>());

    assert_eq!(code(&output), 0, "{}", text(&output));
    assert!(!workspace.config.exists());
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(&custom).unwrap()).unwrap()["pluginHosts"]
            [0]["id"],
        "example-host"
    );

    let relative = workspace.run(&[
        "--config",
        "nested/server.json",
        "plugins",
        "list",
        "--json",
    ]);

    assert_eq!(code(&relative), 0, "{}", text(&relative));
    // Flags valid only for other commands are rejected, not ignored.
    assert_eq!(
        code(&workspace.run(&["--shutdown-on-stdin-close", "plugins", "list"])),
        1
    );
}

#[test]
fn a_concurrent_edit_is_never_overwritten() {
    let host = standard_host();
    let workspace = Workspace::with_config(BASE);

    // Someone else edits the file while the host is being discovered.
    *host.touch.lock().unwrap() = Some(workspace.config.clone());

    let output = run_install(&workspace, &host, &["--all-operations", "--yes"]);

    assert_eq!(code(&output), 1);
    assert!(
        text(&output).contains("changed while it was being edited"),
        "{}",
        text(&output)
    );
    assert_eq!(
        workspace.read(),
        format!("{BASE}\n"),
        "the other writer's version is preserved"
    );
    assert_eq!(
        fs::read_dir(&workspace.root).unwrap().count(),
        1,
        "no temporary file is left behind"
    );
}

#[test]
fn plugin_commands_do_not_link_ffmpeg() {
    let binary = fs::read(env!("CARGO_BIN_EXE_remedia-server")).unwrap();
    let contains = |needle: &[u8]| binary.windows(needle.len()).any(|window| window == needle);

    for library in [
        &b"libavcodec"[..],
        b"libavformat",
        b"libavutil",
        b"libswresample",
        b"avcodec-",
    ] {
        assert!(
            !contains(library),
            "the CLI binary references {}",
            String::from_utf8_lossy(library)
        );
    }

    let workspace = Workspace::with_config(BASE);

    assert_eq!(code(&workspace.run(&["plugins", "list"])), 0);
    let _: &Path = &workspace.root;
}

#[test]
fn a_host_which_never_answers_times_out_without_changes() {
    let host = standard_host();
    let workspace = Workspace::with_config(BASE);

    host.set(Behaviour::Hang);

    let output = run_install(&workspace, &host, &["--all-operations", "--yes"]);

    assert_eq!(code(&output), 1);
    assert!(text(&output).contains("timed out"), "{}", text(&output));
    assert_eq!(workspace.read(), BASE);
}

use super::*;

const PRETTY: &str = r#"{
  "$schema": "https://example.test/schema.json",
  "users": [{ "username": "bot", "password": "env('PASSWORD')" }],
  "pluginHosts": [
    {
      "id": "first",
      "endpoint": "https://first.example",
      "operations": [{ "pluginId": "p", "operation": "o" }]
    },
    {
      "id": "second",
      "endpoint": "https://second.example",
      "operations": [
        { "pluginId": "p", "operation": "a", "replaySafe": true },
        { "pluginId": "p", "operation": "b" }
      ]
    }
  ],
  "observability": { "console": false }
}
"#;

fn entry(id: &str) -> Json {
    Json::object([
        ("id", Json::str(id)),
        ("endpoint", Json::str(format!("https://{id}.example"))),
        (
            "operations",
            Json::Array(vec![Json::object([
                ("pluginId", Json::str("p")),
                ("operation", Json::str("o")),
                ("replaySafe", Json::Bool(false)),
            ])]),
        ),
    ])
}

fn ids(text: &ConfigText) -> Vec<String> {
    let value: Value = serde_json::from_str(text.as_str()).unwrap();

    value["pluginHosts"]
        .as_array()
        .map(|hosts| {
            hosts
                .iter()
                .map(|host| host["id"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn appending_a_host_matches_the_documents_style_and_keeps_everything_else() {
    let edited = ConfigText::new(PRETTY)
        .unwrap()
        .add_host(&entry("third"))
        .unwrap();

    assert_eq!(ids(&edited), ["first", "second", "third"]);
    assert!(edited.as_str().ends_with("}\n"));
    // Everything before the insertion and after the array is byte-identical.
    let (head, _) = PRETTY.split_once("    }\n  ],\n").unwrap();

    assert!(edited.as_str().starts_with(head));
    assert!(
        edited
            .as_str()
            .contains("  ],\n  \"observability\": { \"console\": false }\n}\n")
    );
    assert!(
        edited
            .as_str()
            .contains("    {\n      \"id\": \"third\",\n      \"endpoint\"")
    );
}

#[test]
fn a_missing_array_is_created_after_the_last_member() {
    let source = "{\n    \"users\": []\n}\n";
    let edited = ConfigText::new(source)
        .unwrap()
        .add_host(&entry("only"))
        .unwrap();

    assert_eq!(ids(&edited), ["only"]);
    // Four-space indentation is detected and reused.
    assert!(
        edited
            .as_str()
            .contains("    \"pluginHosts\": [\n        {\n            \"id\": \"only\"")
    );
    assert!(edited.as_str().ends_with("}\n"));
}

#[test]
fn an_empty_array_is_filled_and_compact_documents_stay_compact() {
    let empty = ConfigText::new("{\n  \"pluginHosts\": []\n}").unwrap();

    assert_eq!(ids(&empty.add_host(&entry("a")).unwrap()), ["a"]);

    let compact = ConfigText::new(r#"{"users":[]}"#)
        .unwrap()
        .add_host(&entry("a"))
        .unwrap();

    assert!(!compact.as_str().contains('\n'));
    assert_eq!(ids(&compact), ["a"]);
    assert_eq!(ids(&compact.add_host(&entry("b")).unwrap()), ["a", "b"]);
}

#[test]
fn crlf_and_tab_indentation_are_preserved() {
    let source = "{\r\n\t\"users\": []\r\n}\r\n";
    let edited = ConfigText::new(source)
        .unwrap()
        .add_host(&entry("a"))
        .unwrap();

    assert!(edited.as_str().contains("\r\n\t\t{\r\n\t\t\t\"id\": \"a\""));
    assert!(!edited.as_str().replace("\r\n", "").contains('\n'));
}

#[test]
fn duplicate_ids_are_refused() {
    assert!(matches!(
        ConfigText::new(PRETTY).unwrap().add_host(&entry("first")),
        Err(EditError::HostExists(id)) if id == "first"
    ));
}

#[test]
fn removing_a_host_only_removes_that_entry() {
    let text = ConfigText::new(PRETTY).unwrap();

    for (id, rest) in [("first", ["second"]), ("second", ["first"])] {
        let edited = text.remove_host(id).unwrap();

        assert_eq!(ids(&edited), rest);
        assert!(edited.as_str().contains("\"$schema\""));
        assert!(edited.as_str().ends_with("}\n"));
    }

    let middle = text
        .add_host(&entry("third"))
        .unwrap()
        .remove_host("second")
        .unwrap();

    assert_eq!(ids(&middle), ["first", "third"]);

    let emptied = text
        .remove_host("first")
        .unwrap()
        .remove_host("second")
        .unwrap();

    assert!(emptied.as_str().contains("\"pluginHosts\": []"));
    assert!(matches!(
        emptied.remove_host("first"),
        Err(EditError::HostNotFound(_))
    ));
}

#[test]
fn enabled_is_inserted_toggled_and_never_duplicated() {
    let text = ConfigText::new(PRETTY).unwrap();
    let disabled = text.set_host_enabled("second", false).unwrap();
    let value: Value = serde_json::from_str(disabled.as_str()).unwrap();

    assert_eq!(value["pluginHosts"][1]["enabled"], false);
    assert!(value["pluginHosts"][0].get("enabled").is_none());
    // The first host's text is untouched.
    assert!(
        disabled
            .as_str()
            .contains("\"id\": \"first\",\n      \"endpoint\"")
    );

    let again = disabled.set_host_enabled("second", false).unwrap();

    assert_eq!(again.as_str(), disabled.as_str());
    assert_eq!(again.as_str().matches("\"enabled\"").count(), 1);

    let enabled = disabled.set_host_enabled("second", true).unwrap();
    let value: Value = serde_json::from_str(enabled.as_str()).unwrap();

    assert_eq!(value["pluginHosts"][1]["enabled"], true);
    // Enabling a host which never had the flag is a no-op.
    assert_eq!(
        text.set_host_enabled("first", true).unwrap().as_str(),
        PRETTY
    );
}

#[test]
fn operations_can_be_removed_and_added_without_touching_the_rest() {
    let text = ConfigText::new(PRETTY).unwrap();
    let new = Json::object([
        ("pluginId", Json::str("p")),
        ("operation", Json::str("c")),
        ("replaySafe", Json::Bool(false)),
    ]);
    let edited = text
        .change_host_operations("second", &[("p".into(), "b".into())], &[new])
        .unwrap();
    let value: Value = serde_json::from_str(edited.as_str()).unwrap();
    let operations = value["pluginHosts"][1]["operations"].as_array().unwrap();

    assert_eq!(operations.len(), 2);
    assert_eq!(operations[0]["operation"], "a");
    assert_eq!(operations[0]["replaySafe"], true);
    assert_eq!(operations[1]["operation"], "c");
    assert_eq!(
        value["pluginHosts"][0],
        serde_json::from_str::<Value>(PRETTY).unwrap()["pluginHosts"][0]
    );
}

#[test]
fn malformed_documents_and_non_array_hosts_fail_without_panicking() {
    assert!(ConfigText::new("{ not json").is_err());
    assert!(ConfigText::new("[]").is_err());
    assert!(matches!(
        ConfigText::new(r#"{"pluginHosts":{}}"#)
            .unwrap()
            .add_host(&entry("a")),
        Err(EditError::NotAnArray)
    ));
}

fn scratch() -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "remedia-edit-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));

    fs::create_dir_all(&directory).unwrap();

    directory
}

#[test]
fn atomic_writes_replace_the_file_and_leave_no_temporary_files() {
    let directory = scratch();
    let path = directory.join("remedia.config.json");

    fs::write(&path, PRETTY).unwrap();

    let snapshot = read(&path).unwrap();

    write_atomic(&snapshot, "{\"users\":[]}\n").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"users\":[]}\n");
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_file_changed_after_it_was_read_is_never_overwritten() {
    let directory = scratch();
    let path = directory.join("remedia.config.json");

    fs::write(&path, PRETTY).unwrap();

    let snapshot = read(&path).unwrap();

    fs::write(&path, "{\"users\":[],\"edited\":\"elsewhere\"}\n").unwrap();

    assert!(matches!(
        write_atomic(&snapshot, "{}"),
        Err(EditError::Concurrent(_))
    ));
    assert!(fs::read_to_string(&path).unwrap().contains("elsewhere"));
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn write_failures_preserve_the_original_and_permissions_are_kept() {
    use std::os::unix::fs::PermissionsExt;

    let directory = scratch();
    let path = directory.join("remedia.config.json");

    fs::write(&path, PRETTY).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

    let snapshot = read(&path).unwrap();

    write_atomic(&snapshot, "{\"users\":[]}\n").unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );

    // A directory which cannot hold the temporary file fails before anything is replaced.
    let snapshot = read(&path).unwrap();

    fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).unwrap();

    let blocked = write_atomic(&snapshot, "{\"replaced\":true}\n");

    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();

    if blocked.is_err() {
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"users\":[]}\n");
    }

    fs::remove_dir_all(directory).unwrap();
}

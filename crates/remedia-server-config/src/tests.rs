use std::fs;

use super::*;

const MINIMAL: &str = r#"{"users":[{"username":"bot","password":"env('BOT_PASSWORD')"}]}"#;

fn strict(json: &str) -> Result<Settings, ConfigError> {
    parse(
        json,
        Path::new("."),
        &mut Env::strict(|name| (name == "BOT_PASSWORD").then(|| "secret".to_owned())),
    )
}

#[test]
fn env_references_are_exact_and_do_not_execute_content() {
    let mut env = Env::strict(|name| (name == "VALUE").then(|| "literal $(whoami)".to_owned()));

    assert_eq!(
        resolve_env("field", "env('VALUE')".into(), &mut env).unwrap(),
        "literal $(whoami)"
    );
    assert!(matches!(
        resolve_env("field", "env('VALUE') suffix".into(), &mut env),
        Err(ConfigError::InvalidEnvironmentReference(_))
    ));
    assert!(matches!(
        resolve_env("field", "env('MISSING')".into(), &mut env),
        Err(ConfigError::MissingEnvironment { .. })
    ));
}

#[test]
fn lenient_resolution_validates_structure_without_unrelated_secrets() {
    let json = r#"{"users":[{"username":"bot","password":"env('NOT_SET')"}],
        "pluginHosts":[{"id":"h","endpoint":"env('ALSO_NOT_SET')",
        "auth":{"type":"bearer","token":"env('TOKEN_NOT_SET')"},
        "operations":[{"pluginId":"p","operation":"o"}]}]}"#;
    let mut env = Env::lenient(|_| None);

    assert!(parse(json, Path::new("."), &mut env).is_ok());
    assert_eq!(
        env.unresolved(),
        ["ALSO_NOT_SET", "NOT_SET", "TOKEN_NOT_SET"]
    );
    // Structural errors are still reported in lenient mode.
    assert!(
        parse(
            r#"{"users":[],"extra":1}"#,
            Path::new("."),
            &mut Env::lenient(|_| None)
        )
        .is_err()
    );
    assert!(parse(json, Path::new("."), &mut Env::strict(|_| None)).is_err());
}

#[test]
fn hosts_are_enabled_unless_explicitly_disabled_and_disabled_hosts_are_still_validated() {
    let host = |extra: &str| {
        format!(
            r#"{{"users":[{{"username":"bot","password":"x"}}],"pluginHosts":[{{"id":"h","endpoint":"https://a.example",{extra}"operations":[{{"pluginId":"p","operation":"o"}}]}}]}}"#
        )
    };

    assert!(strict(&host("")).unwrap().plugin_hosts[0].enabled);
    assert!(strict(&host(r#""enabled":true,"#)).unwrap().plugin_hosts[0].enabled);

    let disabled = strict(&host(r#""enabled":false,"#)).unwrap();

    assert!(!disabled.plugin_hosts[0].enabled);
    // A disabled entry keeps its operations, and invalid disabled entries still fail.
    assert_eq!(disabled.plugin_hosts[0].operations.len(), 1);
    assert!(strict(&host(r#""enabled":false,"limits":{"maxAttempts":9},"#)).is_err());
    assert!(strict(&host(r#""enabled":"no","#)).is_err());
}

#[test]
fn home_relative_media_roots_expand_before_resolution() {
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"));

    if let Some(home) = home {
        let home = PathBuf::from(home);
        let json =
            r#"{"media":{"allowedFileRoots":["~"]},"users":[{"username":"bot","password":"x"}]}"#;
        let parsed = strict(json).expect("home directory is a valid media root");

        assert_eq!(
            expand_home_path("~/Music/Downloads").unwrap(),
            home.join("Music/Downloads")
        );
        assert_eq!(
            parsed.media.allowed_file_roots,
            vec![fs::canonicalize(home).unwrap()]
        );
    }

    assert_eq!(expand_home_path("media").unwrap(), PathBuf::from("media"));
}

#[test]
fn rejects_malformed_allowed_host_patterns() {
    assert!(valid_host_pattern("media.example.com"));
    assert!(valid_host_pattern("192.0.2.10"));
    assert!(!valid_host_pattern("https://media.example.com"));
    assert!(!valid_host_pattern("-media.example.com"));
    assert!(!valid_host_pattern("media.*.example.com"));
}

#[test]
fn raw_default_expansion_matches_zod() {
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../../../packages/remedia-config/tests/defaults.json"
    ))
    .unwrap();
    let raw: RawConfig =
        serde_json::from_str(r#"{"users":[{"username":"bot","password":"example"}]}"#).unwrap();
    let mut actual = serde_json::to_value(raw).unwrap();

    actual.as_object_mut().unwrap().remove("$schema");
    assert_eq!(actual, expected);
}

#[test]
fn minimal_configuration_is_valid() {
    assert!(strict(MINIMAL).is_ok());
}

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use super::{
    ConfigError, ServerConfig, host_allowed, is_public_ip, resolve_env, valid_host_pattern,
};

const JSON: &str = r#"{
      "users": [{"username":"bot","password":"env('BOT_PASSWORD')"}]
    }"#;

#[test]
fn defaults_bind_to_loopback_and_load_limits() {
    let config = ServerConfig::parse_with(JSON, Path::new("."), |key| {
        (key == "BOT_PASSWORD").then(|| "secret".to_owned())
    })
    .expect("valid config");
    assert_eq!(config.bind_host, "127.0.0.1");
    assert_eq!(config.port, 8787);
    assert_eq!(config.limits.max_message_bytes, 65_536);
    assert_eq!(config.users[0].password, "secret");
    assert_eq!(config.opus, remedia_core::OpusEncoderConfig::default());
    assert!(!config.crossfade.enabled);
    assert_eq!(config.crossfade.duration_ms, 5_000);
    assert_eq!(config.crossfade.prepare_before_ms, 30_000);
}

#[test]
fn audio_defaults_and_individual_opus_overrides() {
    use remedia_core::OpusEncoderConfig;
    for (audio, expected) in [
        ("{}", OpusEncoderConfig::default()),
        (r#"{"opus":{}}"#, OpusEncoderConfig::default()),
        (
            r#"{"opus":{"bitrate":96000}}"#,
            OpusEncoderConfig {
                bitrate: 96_000,
                ..Default::default()
            },
        ),
        (
            r#"{"opus":{"complexity":0}}"#,
            OpusEncoderConfig {
                complexity: 0,
                ..Default::default()
            },
        ),
        (
            r#"{"opus":{"vbr":false}}"#,
            OpusEncoderConfig {
                vbr: false,
                ..Default::default()
            },
        ),
        (
            r#"{"opus":{"bitrate":256000,"complexity":10,"vbr":false}}"#,
            OpusEncoderConfig {
                bitrate: 256_000,
                complexity: 10,
                vbr: false,
            },
        ),
    ] {
        let json = JSON.replace("\"users\":", &format!("\"audio\":{audio},\"users\":"));
        let config =
            ServerConfig::parse_with(&json, Path::new("."), |_| Some("secret".into())).unwrap();
        assert_eq!(config.opus, expected);
    }
}

#[test]
fn invalid_opus_values_report_the_field_and_unknown_fields_are_rejected() {
    for (field, values) in [
        (
            "bitrate",
            vec![
                "-1",
                "0",
                "499",
                "512001",
                "4294967296",
                "1.5",
                "null",
                "\"128000\"",
            ],
        ),
        (
            "complexity",
            vec!["-1", "11", "4294967296", "1.5", "null", "true"],
        ),
        ("vbr", vec!["0", "null", "\"true\""]),
    ] {
        for value in values {
            let json = JSON.replace(
                "\"users\":",
                &format!("\"audio\":{{\"opus\":{{\"{field}\":{value}}}}},\"users\":"),
            );
            let error = ServerConfig::parse_with(&json, Path::new("."), |_| Some("secret".into()))
                .err()
                .expect("invalid config");
            assert!(
                error.to_string().contains(&format!("audio.opus.{field}")),
                "{error}"
            );
        }
    }
    for audio in [
        r#"{"extra":true}"#,
        r#"{"opus":{"fec":true}}"#,
        r#"{"opus":{"channelCount":1}}"#,
    ] {
        let json = JSON.replace("\"users\":", &format!("\"audio\":{audio},\"users\":"));
        let error = ServerConfig::parse_with(&json, Path::new("."), |_| Some("secret".into()))
            .err()
            .expect("unknown field");
        assert!(error.to_string().contains("unknown field"), "{error}");
    }
}

#[test]
fn rejects_missing_env_unknown_keys_and_bad_values() {
    assert!(matches!(
        ServerConfig::parse_with(JSON, Path::new("."), |_| None),
        Err(ConfigError::MissingEnvironment { .. })
    ));
    let unknown = JSON.replace("\"users\":", "\"extra\":true,\"users\":");
    assert!(ServerConfig::parse_with(&unknown, Path::new("."), |_| Some("x".into())).is_err());
    let legacy_bot_config = JSON.replace(
        "\"password\":\"env('BOT_PASSWORD')\"",
        "\"password\":\"env('BOT_PASSWORD')\",\"discordBotUserId\":\"123456789012345678\"",
    );
    assert!(
        ServerConfig::parse_with(&legacy_bot_config, Path::new("."), |_| Some("x".into())).is_err()
    );
    let malformed = JSON.replace("env('BOT_PASSWORD')", "env(BOT_PASSWORD)");
    assert!(matches!(
        ServerConfig::parse_with(&malformed, Path::new("."), |_| Some("x".into())),
        Err(ConfigError::InvalidEnvironmentReference(_))
    ));
}

#[test]
fn env_references_are_exact_and_do_not_execute_content() {
    let mut lookup = |name: &str| (name == "VALUE").then(|| "literal $(whoami)".to_owned());
    assert_eq!(
        resolve_env("field", "env('VALUE')".into(), &mut lookup).unwrap(),
        "literal $(whoami)"
    );
    assert!(matches!(
        resolve_env("field", "env('VALUE') suffix".into(), &mut lookup),
        Err(ConfigError::InvalidEnvironmentReference(_))
    ));
}

#[test]
fn home_relative_media_roots_expand_before_resolution() {
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"));
    if let Some(home) = home {
        let home = PathBuf::from(home);
        let json = JSON.replace(
            "\"users\":",
            "\"media\":{\"allowedFileRoots\":[\"~\"]},\"users\":",
        );
        let parsed = ServerConfig::parse_with(&json, Path::new("."), |_| Some("x".into()))
            .expect("home directory is a valid media root");
        assert_eq!(
            super::expand_home_path("~/Music/Downloads").unwrap(),
            home.join("Music/Downloads")
        );
        assert_eq!(
            parsed.media.allowed_file_roots,
            vec![fs::canonicalize(home).unwrap()]
        );
    }
    assert_eq!(
        super::expand_home_path("media").unwrap(),
        PathBuf::from("media")
    );
}

#[test]
fn missing_config_error_reports_the_requested_path() {
    let path = env::temp_dir().join(format!(
        "remedia-missing-config-{}.json",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    match ServerConfig::from_file(&path) {
        Err(ConfigError::Io { path: reported, .. }) => assert_eq!(reported, path),
        Err(error) => panic!("expected path-aware read error, got {error}"),
        Ok(_) => panic!("missing config path unexpectedly loaded"),
    }
}

#[test]
fn private_and_special_ip_ranges_are_rejected() {
    for address in [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.2.3",
        "192.168.1.2",
        "169.254.169.254",
        "::1",
        "fc00::1",
        "fe80::1",
        "2001:db8::1",
    ] {
        assert!(!is_public_ip(address.parse().unwrap()), "{address}");
    }
    assert!(is_public_ip("8.8.8.8".parse().unwrap()));
    assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
}

#[test]
fn remote_hosts_require_explicit_allowlist_or_explicit_wildcard() {
    assert!(!host_allowed("media.example.test", &[]));
    assert!(host_allowed("media.example.test", &["*".into()]));
    assert!(host_allowed("cdn.example.test", &["example.test".into()]));
    assert!(!host_allowed(
        "example-attacker.test",
        &["example.test".into()]
    ));
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
fn crossfade_configuration_requires_explicit_opt_in_and_valid_bounds() {
    let json = JSON.replace(
        "\"users\":",
        "\"audio\":{\"crossfade\":{\"enabled\":true,\"durationMs\":8000,\"prepareBeforeMs\":45000}},\"users\":",
    );
    let config = ServerConfig::parse_with(&json, Path::new("."), |_| Some("secret".into()))
        .expect("valid crossfade settings");
    assert!(config.crossfade.enabled);
    assert_eq!(config.crossfade.duration_ms, 8_000);
    assert_eq!(config.crossfade.prepare_before_ms, 45_000);

    for crossfade in [
        r#"{"durationMs":0}"#,
        r#"{"durationMs":30001}"#,
        r#"{"prepareBeforeMs":100}"#,
        r#"{"prepareBeforeMs":120001}"#,
        r#"{"curve":"linear"}"#,
        r#"{"unknown":true}"#,
    ] {
        let json = JSON.replace(
            "\"users\":",
            &format!("\"audio\":{{\"crossfade\":{crossfade}}},\"users\":"),
        );
        assert!(
            ServerConfig::parse_with(&json, Path::new("."), |_| Some("secret".into())).is_err(),
            "must reject crossfade: {crossfade}"
        );
    }
}

fn with_plugin_hosts(hosts: &str) -> String {
    format!(r#"{{"users":[{{"username":"bot","password":"secret"}}],"pluginHosts":{hosts}}}"#)
}

fn parse_hosts(hosts: &str) -> Result<ServerConfig, ConfigError> {
    ServerConfig::parse_with(&with_plugin_hosts(hosts), Path::new("."), |key| match key {
        "HOST_TOKEN" => Some("0123456789abcdef-token".to_owned()),
        "HOST_URL" => Some("https://sources.internal.example".to_owned()),
        _ => None,
    })
}

const HOST: &str = r#"{"id":"source-host-1","endpoint":"https://sources.example.com",
    "operations":[{"pluginId":"example.source","operation":"source.stream"}]}"#;

#[test]
fn plugin_hosts_are_optional_and_use_conservative_defaults() {
    assert!(
        ServerConfig::parse_with(JSON, Path::new("."), |_| Some("secret".into()))
            .unwrap()
            .plugin_hosts
            .is_empty()
    );

    let config = parse_hosts(&format!("[{HOST}]")).unwrap();
    let host = &config.plugin_hosts[0];

    assert_eq!(host.id, "source-host-1");
    assert!(host.token.is_none());
    assert!(!host.allow_insecure_http);
    assert!(!host.allow_private_network);
    assert!(!host.allow_external_media);
    assert_eq!(host.media_origins, ["https://sources.example.com"]);
    assert_eq!(host.limits.max_request_bytes, 65_536);
    assert_eq!(host.limits.max_pending_operations, 8);
    assert_eq!(host.limits.max_attempts, 2);
    assert!(!host.operations[0].replay_safe);
}

#[test]
fn plugin_host_secrets_and_endpoints_support_environment_references_and_stay_redacted() {
    let config = parse_hosts(
        r#"[{"id":"h","endpoint":"env('HOST_URL')",
            "auth":{"type":"bearer","token":"env('HOST_TOKEN')"},
            "mediaOrigins":["https://cdn.internal.example"],
            "operations":[{"pluginId":"p","operation":"o","replaySafe":true}]}]"#,
    )
    .unwrap();
    let host = &config.plugin_hosts[0];

    assert_eq!(host.endpoint.as_str(), "https://sources.internal.example/");
    assert_eq!(
        host.token.as_ref().unwrap().expose(),
        "0123456789abcdef-token"
    );
    assert_eq!(
        host.media_origins,
        [
            "https://sources.internal.example",
            "https://cdn.internal.example"
        ]
    );
    assert!(host.operations[0].replay_safe);
    assert!(!format!("{host:?}").contains("0123456789abcdef-token"));
    assert!(matches!(
        parse_hosts(
            r#"[{"id":"h","endpoint":"env('MISSING')","operations":[{"pluginId":"p","operation":"o"}]}]"#
        ),
        Err(ConfigError::MissingEnvironment { .. })
    ));
}

#[test]
fn plugin_host_registrations_are_strictly_validated() {
    let operation = r#""operations":[{"pluginId":"p","operation":"o"}]"#;
    let invalid = [
        // Duplicate host IDs fail startup.
        format!("[{HOST},{HOST}]"),
        // Unknown fields, bad IDs and endpoints.
        format!(r#"[{{"id":"h","endpoint":"https://a.example","extra":1,{operation}}}]"#),
        format!(r#"[{{"id":"bad id","endpoint":"https://a.example",{operation}}}]"#),
        format!(r#"[{{"id":"h","endpoint":"ftp://a.example",{operation}}}]"#),
        format!(r#"[{{"id":"h","endpoint":"https://a.example/v1",{operation}}}]"#),
        format!(r#"[{{"id":"h","endpoint":"https://u:p@a.example",{operation}}}]"#),
        format!(r#"[{{"id":"h","endpoint":"https://a.example/?q=1",{operation}}}]"#),
        // Cleartext HTTP and private literals require explicit opt-ins.
        format!(r#"[{{"id":"h","endpoint":"http://a.example",{operation}}}]"#),
        format!(r#"[{{"id":"h","endpoint":"https://127.0.0.1:9",{operation}}}]"#),
        // Credentials and operations.
        format!(r#"[{{"id":"h","endpoint":"https://a.example","auth":{{"type":"bearer","token":"short"}},{operation}}}]"#),
        format!(r#"[{{"id":"h","endpoint":"https://a.example","auth":{{"type":"basic"}},{operation}}}]"#),
        r#"[{"id":"h","endpoint":"https://a.example","operations":[]}]"#.to_owned(),
        r#"[{"id":"h","endpoint":"https://a.example"}]"#.to_owned(),
        r#"[{"id":"h","endpoint":"https://a.example","operations":[{"pluginId":"p","operation":"o"},{"pluginId":"p","operation":"o"}]}]"#.to_owned(),
        r#"[{"id":"h","endpoint":"https://a.example","operations":[{"pluginId":"p/x","operation":"o"}]}]"#.to_owned(),
        // Limits.
        format!(r#"[{{"id":"h","endpoint":"https://a.example","limits":{{"maxAttempts":9}},{operation}}}]"#),
        format!(r#"[{{"id":"h","endpoint":"https://a.example","limits":{{"maxActiveStreams":0}},{operation}}}]"#),
        format!(r#"[{{"id":"h","endpoint":"https://a.example","limits":{{"requestTimeoutMs":0}},{operation}}}]"#),
    ];

    for hosts in invalid {
        assert!(parse_hosts(&hosts).is_err(), "{hosts} should be rejected");
    }

    let allowed = parse_hosts(&format!(
        r#"[{{"id":"h","endpoint":"http://127.0.0.1:9","allowInsecureHttp":true,"allowPrivateNetwork":true,{operation}}}]"#
    ))
    .unwrap();

    assert!(allowed.plugin_hosts[0].allow_insecure_http);
    assert!(allowed.plugin_hosts[0].allow_private_network);
}

#[test]
fn shared_zod_validation_corpus_matches_rust() {
    let cases: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../packages/remedia-config/tests/parity.json"
    ))
    .unwrap();
    for case in cases.as_array().unwrap() {
        let valid =
            ServerConfig::parse_with(
                &case["config"].to_string(),
                Path::new("."),
                |name| match name {
                    "USER" => Some("bot".into()),
                    "PASSWORD" => Some("secret".into()),
                    _ => None,
                },
            )
            .is_ok();
        assert_eq!(valid, case["valid"].as_bool().unwrap(), "{}", case["name"]);
    }
}

#[test]
fn raw_default_expansion_matches_zod() {
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../packages/remedia-config/tests/defaults.json"
    ))
    .unwrap();
    let raw: super::raw::RawConfig =
        serde_json::from_str(r#"{"users":[{"username":"bot","password":"example"}]}"#).unwrap();
    let mut actual = serde_json::to_value(raw).unwrap();
    actual.as_object_mut().unwrap().remove("$schema");
    assert_eq!(actual, expected);
}

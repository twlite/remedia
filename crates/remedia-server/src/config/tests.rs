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

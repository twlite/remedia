use super::{claim_voice_generation, parse_snowflake, valid_discord_voice_endpoint};
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn snowflakes_are_decimal_and_remain_u64() {
    assert_eq!(parse_snowflake("18446744073709551614"), Some(u64::MAX - 1));
    assert_eq!(parse_snowflake("1.2"), None);
    assert_eq!(parse_snowflake("18446744073709551616"), None);
}

#[test]
fn host_stats_are_optional_and_well_formed_when_available() {
    assert!(
        remedia_resources::collect()
            .memory
            .resident_bytes
            .is_none_or(|memory| memory > 0)
    );
    assert!(
        remedia_resources::collect()
            .cpu
            .load_average_1m
            .is_none_or(|load| load.is_finite() && load >= 0.0)
    );
}

#[test]
fn voice_endpoints_are_secure_and_scoped_to_discord_media() {
    assert!(valid_discord_voice_endpoint("us-east123.discord.media"));
    assert!(valid_discord_voice_endpoint(
        "wss://voice.discord.media:2048"
    ));
    assert!(!valid_discord_voice_endpoint("ws://voice.discord.media"));
    assert!(!valid_discord_voice_endpoint("wss://127.0.0.1:8080"));
    assert!(!valid_discord_voice_endpoint(
        "wss://discord.media.attacker.test"
    ));
    assert!(!valid_discord_voice_endpoint(
        "wss://user@voice.discord.media"
    ));
}

#[test]
fn voice_generations_reject_stale_operations_and_allow_same_attempt_steps() {
    let generation = AtomicU64::new(0);
    assert_eq!(claim_voice_generation(&generation, Some(4)).unwrap(), 4);
    assert_eq!(claim_voice_generation(&generation, Some(4)).unwrap(), 4);
    assert_eq!(generation.load(Ordering::Acquire), 4);
    assert!(claim_voice_generation(&generation, Some(3)).is_err());
    assert_eq!(claim_voice_generation(&generation, None).unwrap(), 5);
    assert_eq!(claim_voice_generation(&generation, Some(9)).unwrap(), 9);
    assert!(claim_voice_generation(&generation, Some(0)).is_err());
}

use super::*;

pub(crate) fn node_stats(server: &ServerInner) -> Value {
    let (processing_failed, transport_failed, active_playbacks) = {
        let sessions = lock(&server.sessions);
        sessions
            .values()
            .flat_map(|session| lock(&session.guilds).values().cloned().collect::<Vec<_>>())
            .fold(
                (false, false, 0),
                |(processing_failed, transport_failed, active), guild| {
                    let playback_failed = guild.playback.state() == PlaybackState::Failed;
                    let voice_failed = lock(&guild.voice)
                        .as_ref()
                        .is_some_and(|voice| voice.state() == ConnectionState::Failed);
                    (
                        processing_failed || playback_failed,
                        transport_failed || voice_failed,
                        active
                            + usize::from(matches!(
                                guild.playback.state(),
                                PlaybackState::Starting
                                    | PlaybackState::Playing
                                    | PlaybackState::Pausing
                                    | PlaybackState::Paused
                                    | PlaybackState::Resuming
                                    | PlaybackState::Stopping
                            )),
                    )
                },
            )
    };
    let resources = remedia_resources::cached_snapshot();
    json!({
        "nodeId": server.node_id,
        "version": env!("REMEDIA_BUILD_VERSION"),
        "commit": env!("REMEDIA_BUILD_COMMIT"),
        "protocolVersion": crate::protocol::PROTOCOL_VERSION,
        "uptimeMs": server.started_at.elapsed().as_millis(),
        "connections": {"active": server.active_connections.load(Ordering::Acquire), "maximum": server.config.limits.max_connections},
        "players": {"active": server.active_player_count(), "maximum": server.config.limits.max_total_players, "available": server.config.limits.max_total_players.saturating_sub(server.active_player_count())},
        "playback": {"activeSessions": active_playbacks},
        "cpu": resources.cpu,
        "memory": resources.memory,
        "resources": resources,
        "pluginHosts": server.plugin_hosts.snapshot(),
        "transportHealth": if transport_failed { "degraded" } else { "healthy" },
        "processingHealth": if processing_failed { "degraded" } else { "healthy" },
    })
}

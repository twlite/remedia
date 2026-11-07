use super::*;

pub(crate) fn node_stats(server: &ServerInner) -> Value {
    let (processing_failed, transport_failed) = {
        let sessions = lock(&server.sessions);
        sessions
            .values()
            .flat_map(|session| lock(&session.guilds).values().cloned().collect::<Vec<_>>())
            .fold(
                (false, false),
                |(processing_failed, transport_failed), guild| {
                    let playback_failed = guild.playback.state() == PlaybackState::Failed;
                    let voice_failed = lock(&guild.voice)
                        .as_ref()
                        .is_some_and(|voice| voice.state() == ConnectionState::Failed);
                    (
                        processing_failed || playback_failed,
                        transport_failed || voice_failed,
                    )
                },
            )
    };
    json!({
        "nodeId": server.node_id,
        "version": env!("CARGO_PKG_VERSION"),
        "protocolVersion": crate::protocol::PROTOCOL_VERSION,
        "uptimeMs": server.started_at.elapsed().as_millis(),
        "connections": {"active": server.active_connections.load(Ordering::Acquire), "maximum": server.config.limits.max_connections},
        "players": {"active": server.active_player_count(), "maximum": server.config.limits.max_total_players},
        "cpu": {"availableParallelism": std::thread::available_parallelism().ok().map(usize::from), "loadAverage1m": load_average_1m()},
        "memory": {"residentBytes": resident_memory_bytes()},
        "transportHealth": if transport_failed { "degraded" } else { "healthy" },
        "processingHealth": if processing_failed { "degraded" } else { "healthy" },
    })
}

pub(super) fn load_average_1m() -> Option<f64> {
    let contents = std::fs::read_to_string("/proc/loadavg").ok()?;
    contents.split_whitespace().next()?.parse().ok()
}

pub(super) fn resident_memory_bytes() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = contents.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

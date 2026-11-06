use super::*;

impl ClientSession {
    pub(crate) fn ensure_open(&self) -> Result<(), StateError> {
        if self.closed.load(Ordering::Acquire) {
            Err(StateError::UnknownSession)
        } else {
            Ok(())
        }
    }

    pub(crate) fn attached_to(&self, connection_id: &str) -> bool {
        lock(&self.attached_connection).as_deref() == Some(connection_id)
            && !self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<serde_json::Value> {
        lock(&self.initial_event_rx)
            .take()
            .unwrap_or_else(|| self.event_tx.subscribe())
    }

    pub(crate) fn emit(&self, name: &str, data: serde_json::Value) {
        let sequence = self.event_sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.event_tx.send(event(name, &self.id, sequence, data));
    }

    pub(crate) fn guild_snapshots(&self) -> Vec<serde_json::Value> {
        lock(&self.guilds)
            .values()
            .map(|guild| guild.snapshot())
            .collect()
    }

    pub(crate) fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "sessionId": self.id,
            "botUserId": self.bot_user_id.to_string(),
            "guilds": self.guild_snapshots(),
        })
    }
}

impl GuildSession {
    pub(crate) fn ensure_open(&self) -> Result<(), StateError> {
        if self.closed.load(Ordering::Acquire) {
            Err(StateError::UnknownSession)
        } else {
            Ok(())
        }
    }

    /// Claim the single terminal lifecycle notification for an RPC source generation.
    pub(crate) fn claim_terminal_event(&self, generation: u64) -> bool {
        let mut previous = self.terminal_generation.load(Ordering::Acquire);
        loop {
            if previous >= generation {
                return false;
            }
            match self.terminal_generation.compare_exchange_weak(
                previous,
                generation,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(current) => previous = current,
            }
        }
    }

    pub(crate) fn snapshot(&self) -> serde_json::Value {
        let voice = lock(&self.voice);
        let voice_state = if self.voice_reconnecting.load(Ordering::Acquire) {
            Some("reconnecting")
        } else {
            voice.as_ref().map(|voice| match voice.state() {
                remedia_discord::ConnectionState::NegotiatingDave => "negotiating_dave",
                remedia_discord::ConnectionState::Connected => "connected",
                remedia_discord::ConnectionState::Reconnecting => "reconnecting",
                remedia_discord::ConnectionState::Disconnecting => "disconnecting",
                remedia_discord::ConnectionState::Disconnected => "disconnected",
                remedia_discord::ConnectionState::Failed => "failed",
            })
        };
        serde_json::json!({
            "guildSessionId": self.id,
            "guildId": self.guild_id,
            "botUserId": self.bot_user_id.to_string(),
            "playback": playback_snapshot(
                &self.playback,
                self.source_generation.load(Ordering::Acquire),
            ),
            "voiceState": voice_state,
        })
    }
}

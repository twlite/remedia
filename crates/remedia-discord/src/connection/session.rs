use super::*;

impl DiscordVoiceConnection {
    /// Connect to Discord voice using session information from an external
    /// Gateway client. This performs the voice WebSocket handshake, UDP IP
    /// discovery, encryption selection, and DAVE setup.
    pub async fn connect(info: VoiceConnectionInfo) -> Result<Self, VoiceError> {
        Self::connect_with_observability(info, ObservabilityHandle::default()).await
    }

    /// Connect using an application-owned Remedia observability context.
    ///
    /// The context is attached only to this connection's work and does not install or replace a
    /// process-global tracing subscriber.
    pub async fn connect_with_observability(
        info: VoiceConnectionInfo,
        observability: ObservabilityHandle,
    ) -> Result<Self, VoiceError> {
        let voice_connection_id = next_correlation_id();
        let guild_id = info.guild_id;
        let started_at = Instant::now();
        let metrics = observability.metrics();
        metrics.voice_connection_attempt();
        let dispatch = observability.dispatch().cloned();
        let span = with_dispatch(&observability, || {
            tracing::info_span!(
                target: "remedia_discord::connection",
                "discord.connection.establish",
                voice_connection_id,
                guild_id,
                otel.status_code = tracing::field::Empty,
                error.type = tracing::field::Empty,
            )
        });
        let run = async move {
            tracing::info!(
                name: "discord.connection.attempt",
                target: "remedia_discord::connection",
                voice_connection_id,
                "Discord voice connection attempt started"
            );
            let result = Self::connect_inner(
                info,
                observability,
                voice_connection_id,
                started_at,
                metrics.clone(),
            )
            .await;
            if let Err(error) = &result {
                tracing::Span::current().record("otel.status_code", "ERROR");
                tracing::Span::current().record("error.type", error.kind());
                tracing::error!(
                    name: "discord.connection.failed",
                    target: "remedia_discord::connection",
                    voice_connection_id,
                    error_kind = error.kind(),
                    "Discord voice connection attempt failed"
                );
            } else {
                tracing::Span::current().record("otel.status_code", "OK");
            }
            result
        }
        .instrument(span);
        match dispatch {
            Some(dispatch) => run.with_subscriber(dispatch).await,
            None => run.await,
        }
    }

    pub(super) async fn connect_inner(
        info: VoiceConnectionInfo,
        observability: ObservabilityHandle,
        connection_id: u64,
        started_at: Instant,
        metrics: remedia_observability::RemediaMetrics,
    ) -> Result<Self, VoiceError> {
        let endpoint = voice_endpoint(&info.endpoint)?;
        let guild_id = info.guild_id;
        let (mut websocket, hello_interval, ready, early_messages, session_data, udp, sequence_ack) =
            establish_voice_session(&endpoint, &info, connection_id).await?;

        let dave_version = session_data
            .get("dave_protocol_version")
            .map(|value| parse_u16(value, "dave_protocol_version"))
            .transpose()?
            .ok_or_else(|| {
                VoiceError::Protocol(
                    "voice session description omitted `dave_protocol_version`".into(),
                )
            })?;
        if dave_version != DAVE_PROTOCOL_VERSION {
            return Err(VoiceError::UnsupportedDaveVersion(dave_version));
        }
        tracing::info!(
            name: "discord.dave.protocol_negotiated",
            target: "remedia_discord::connection",
            voice_connection_id = connection_id,
            protocol_version = dave_version,
            "mandatory DAVE protocol version negotiated"
        );
        let secret_key = parse_secret_key(&session_data)?;
        let mode_name = session_data
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                VoiceError::Protocol("session description omitted encryption mode".into())
            })?;
        let mode = EncryptionMode::choose(&ready.modes)?;
        if mode.wire_name() != mode_name {
            return Err(VoiceError::EncryptionModeMismatch);
        }
        tracing::info!(
            name: "discord.transport.encryption_negotiated",
            target: "remedia_discord::connection",
            voice_connection_id = connection_id,
            mode = mode.wire_name(),
            "voice transport encryption mode negotiated"
        );

        let dave_span = tracing::info_span!(
            target: "remedia_discord::connection",
            "discord.dave.initialize",
            voice_connection_id = connection_id,
            protocol_version = dave_version,
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let dave_init = dave_span.in_scope(|| {
            let mut dave = DaveProtocol::new(dave_version, info.user_id, info.channel_id)?;
            let key_package = dave.create_key_package()?;
            Ok::<_, VoiceError>((dave, key_package))
        });
        let (dave, initial_key_package) = match dave_init {
            Ok(initialized) => {
                dave_span.record("otel.status_code", "OK");
                initialized
            }
            Err(error) => {
                dave_span.record("otel.status_code", "ERROR");
                dave_span.record("error.type", error.kind());
                tracing::error!(
                    name: "discord.dave.initialization_failed",
                    target: "remedia_discord::connection",
                    voice_connection_id = connection_id,
                    error_kind = error.kind(),
                    "DAVE session initialization failed"
                );
                return Err(error);
            }
        };
        if let Err(error) =
            send_ws_message(&mut websocket, Message::Binary(initial_key_package.into()))
                .instrument(dave_span.clone())
                .await
        {
            dave_span.record("otel.status_code", "ERROR");
            dave_span.record("error.type", error.kind());
            tracing::error!(
                name: "discord.dave.initialization_failed",
                target: "remedia_discord::connection",
                voice_connection_id = connection_id,
                error_kind = error.kind(),
                "DAVE key package could not be sent"
            );
            return Err(error);
        }
        dave_span.record("otel.status_code", "OK");
        tracing::info!(
            name: "discord.dave.initialized",
            target: "remedia_discord::connection",
            voice_connection_id = connection_id,
            protocol_version = dave_version,
            "DAVE session initialized; awaiting MLS readiness"
        );
        let dave = Arc::new(Mutex::new(dave));

        // Some gateways can send DAVE events close to session establishment.
        // Keep their order and replay them through the same state machine.
        let pending_gateway_messages = early_messages;

        let socket_addr = SocketAddr::new(ready.ip, ready.port);
        let rtp = RtpSender::new(ready.ssrc, random(), random(), random(), mode, &secret_key)?;

        let (packet_tx, packet_rx) = flume::bounded(QUEUE_CAPACITY);
        let (control_tx, control_rx) = flume::bounded(16);
        let (generation_tx, generation_rx) = watch::channel(0);
        let cancelled_generation = Arc::new(AtomicU64::new(u64::MAX));
        let (state, _) = watch::channel(ConnectionState::NegotiatingDave);
        let cancellation = TokioCancellationToken::new();
        let failure = Arc::new(Mutex::new(None));
        let actor_failure = Arc::clone(&failure);
        let actor_state = state.clone();
        let actor_cancel = cancellation.clone();
        let actor_cancelled_generation = Arc::clone(&cancelled_generation);
        let actor_id = connection_id;
        let actor_guild_id = info.guild_id;
        let actor_metrics = metrics.clone();
        let actor_span = with_dispatch(&observability, || {
            tracing::info_span!(
                target: "remedia_discord::connection",
                "discord.connection.worker",
                voice_connection_id = actor_id,
                guild_id = actor_guild_id,
                otel.status_code = tracing::field::Empty,
                error.type = tracing::field::Empty,
            )
        });
        let actor_future = async move {
            let result = voice_actor(VoiceActorSetup {
                voice_connection_id: actor_id,
                metrics: actor_metrics.clone(),
                active_guard: ActiveVoiceConnection::new(actor_metrics),
                websocket,
                udp,
                udp_server: socket_addr,
                info,
                ssrc: ready.ssrc,
                heartbeat_interval: hello_interval,
                pending_gateway_messages,
                sequence_ack,
                dave,
                rtp,
                packet_rx,
                control_rx,
                generation_rx,
                cancelled_generation: actor_cancelled_generation,
                cancellation: actor_cancel,
                state: actor_state.clone(),
            })
            .await;
            if let Err(error) = result {
                tracing::Span::current().record("otel.status_code", "ERROR");
                tracing::Span::current().record("error.type", error.kind());
                tracing::error!(
                    name: "discord.connection.worker_failed",
                    target: "remedia_discord::connection",
                    voice_connection_id = actor_id,
                    error_kind = error.kind(),
                    "Discord voice connection worker failed"
                );
                *lock(&actor_failure) = Some(Arc::new(error));
                set_connection_state(
                    &actor_state,
                    ConnectionState::Failed,
                    actor_id,
                    actor_guild_id,
                );
            } else {
                tracing::Span::current().record("otel.status_code", "OK");
                if actor_state.borrow().eq(&ConnectionState::Connected) {
                    set_connection_state(
                        &actor_state,
                        ConnectionState::Disconnected,
                        actor_id,
                        actor_guild_id,
                    );
                }
            }
        }
        .instrument(actor_span);
        let actor_dispatch = observability.dispatch().cloned();
        let worker = match actor_dispatch {
            Some(dispatch) => tokio::spawn(actor_future.with_subscriber(dispatch)),
            None => tokio::spawn(actor_future),
        };

        let connection = Self {
            inner: Arc::new(ConnectionInner {
                voice_connection_id: connection_id,
                guild_id,
                observability: observability.clone(),
                state,
                failure,
                packet_tx,
                control_tx,
                generation: AtomicU64::new(0),
                generation_update: Mutex::new(()),
                generation_tx,
                cancelled_generation,
                accepted_packet_id: AtomicU64::new(0),
                next_packet_id: AtomicU64::new(0),
                cancellation,
                worker: Mutex::new(Some(worker)),
            }),
        };
        let mut state_rx = connection.inner.state.subscribe();
        let readiness = time::timeout(Duration::from_secs(20), async {
            loop {
                match *state_rx.borrow_and_update() {
                    ConnectionState::Connected => return Ok(()),
                    ConnectionState::Failed => {
                        return Err(VoiceError::Worker(connection.failure().map_or_else(
                            || "DAVE setup failed".into(),
                            |error| error.to_string(),
                        )));
                    }
                    ConnectionState::Disconnected => return Err(VoiceError::Disconnected),
                    ConnectionState::NegotiatingDave
                    | ConnectionState::Reconnecting
                    | ConnectionState::Disconnecting => {}
                }
                state_rx
                    .changed()
                    .await
                    .map_err(|_| VoiceError::Disconnected)?;
            }
        })
        .await;
        match readiness {
            Ok(Ok(())) => {
                let establishment_seconds = started_at.elapsed().as_secs_f64();
                metrics.voice_connection_established(establishment_seconds);
                tracing::info!(
                    name: "discord.connection.established",
                    target: "remedia_discord::connection",
                    voice_connection_id = connection_id,
                    establishment_seconds,
                    "Discord voice transport established with DAVE ready"
                );
                Ok(connection)
            }
            Ok(Err(error)) => {
                connection.inner.cancellation.cancel();
                connection.join_worker().await;
                Err(error)
            }
            Err(_) => {
                connection.inner.cancellation.cancel();
                connection.join_worker().await;
                Err(VoiceError::Timeout("waiting for mandatory DAVE setup"))
            }
        }
    }

    /// Read the current transport state.
    pub fn state(&self) -> ConnectionState {
        *self.inner.state.borrow()
    }

    /// Return this connection's internal, process-unique diagnostic correlation ID.
    /// This is unrelated to Discord's voice session ID.
    pub fn voice_connection_id(&self) -> u64 {
        self.inner.voice_connection_id
    }

    pub(crate) fn guild_id(&self) -> u64 {
        self.inner.guild_id
    }

    /// Return the latest transport failure, if the connection failed.
    pub fn failure(&self) -> Option<Arc<VoiceError>> {
        lock(&self.inner.failure).clone()
    }

    /// Create a bounded, synchronous Opus sink for one playback output.
    ///
    /// Creating a new sender supersedes the previous sender on this
    /// connection. Cancelling or finishing this sender leaves the voice
    /// connection open. RTP sequence, timestamp, and nonce state remain owned
    /// by the connection across output instances.
    pub fn opus_sender(&self) -> DiscordOpusSender {
        let _generation_update = lock(&self.inner.generation_update);
        let generation = self.inner.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.inner.generation_tx.send_replace(generation);
        with_dispatch(&self.inner.observability, || {
            tracing::info!(
                name: "discord.rtp_sender.started",
                target: "remedia_discord::connection",
                voice_connection_id = self.inner.voice_connection_id,
                generation,
                "playback-scoped Opus sender started"
            );
        });
        DiscordOpusSender {
            inner: Arc::clone(&self.inner),
            generation: AtomicU64::new(generation),
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        }
    }

    /// Close the transport worker and wait for its sockets and DAVE state to be
    /// released. This operation is idempotent.
    pub async fn disconnect(&self) -> Result<(), VoiceError> {
        let dispatch = self.inner.observability.dispatch().cloned();
        let run = with_dispatch(&self.inner.observability, || self.disconnect_inner());
        match dispatch {
            Some(dispatch) => run.with_subscriber(dispatch).await,
            None => run.await,
        }
    }

    #[tracing::instrument(
        name = "discord.connection.disconnect",
        target = "remedia_discord::connection",
        skip(self),
        fields(
            voice_connection_id = self.inner.voice_connection_id,
            guild_id = self.inner.guild_id
        )
    )]
    pub(super) async fn disconnect_inner(&self) -> Result<(), VoiceError> {
        if self.state() == ConnectionState::Disconnected {
            self.join_worker().await;
            return Ok(());
        }
        if self.state() == ConnectionState::Failed {
            self.inner.cancellation.cancel();
            self.join_worker().await;
            return Ok(());
        }

        with_dispatch(&self.inner.observability, || {
            set_connection_state(
                &self.inner.state,
                ConnectionState::Disconnecting,
                self.inner.voice_connection_id,
                self.inner.guild_id,
            );
            tracing::info!(
                name: "discord.connection.disconnect_requested",
                target: "remedia_discord::connection",
                voice_connection_id = self.inner.voice_connection_id,
                "voice transport disconnection requested"
            );
        });
        self.inner.cancellation.cancel();
        self.join_worker().await;
        Ok(())
    }

    pub(super) async fn join_worker(&self) {
        let worker = lock(&self.inner.worker).take();
        if let Some(worker) = worker {
            if let Err(error) = worker.await {
                *lock(&self.inner.failure) = Some(Arc::new(VoiceError::Worker(error.to_string())));
                with_dispatch(&self.inner.observability, || {
                    set_connection_state(
                        &self.inner.state,
                        ConnectionState::Failed,
                        self.inner.voice_connection_id,
                        self.inner.guild_id,
                    );
                });
            }
            return;
        }

        let mut state = self.inner.state.subscribe();
        loop {
            if matches!(
                *state.borrow_and_update(),
                ConnectionState::Disconnected | ConnectionState::Failed
            ) {
                return;
            }
            if state.changed().await.is_err() {
                return;
            }
        }
    }
}

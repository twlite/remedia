use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

/// Remedia's low-cardinality metric instruments.
///
/// The no-op default performs no provider installation. Applications can use
/// [`Observability::handle`](crate::Observability::handle) to share one configured set of
/// instruments across independent playback and voice connections.
#[derive(Clone, Default)]
pub struct RemediaMetrics {
    playback_active: Option<UpDownCounter<i64>>,
    playback_starts: Option<Counter<u64>>,
    playback_completions: Option<Counter<u64>>,
    playback_failures: Option<Counter<u64>>,
    decoder_startup_seconds: Option<Histogram<f64>>,
    voice_connections_active: Option<UpDownCounter<i64>>,
    voice_connection_attempts: Option<Counter<u64>>,
    voice_reconnect_attempts: Option<Counter<u64>>,
    rtp_packets_sent: Option<Counter<u64>>,
    rtp_packets_dropped: Option<Counter<u64>>,
    packet_underruns: Option<Counter<u64>>,
    connection_establishment_seconds: Option<Histogram<f64>>,
    scheduling_delay_seconds: Option<Histogram<f64>>,
}

impl RemediaMetrics {
    /// Create Remedia metric instruments from an application-owned OpenTelemetry meter.
    pub fn from_meter(meter: Meter) -> Self {
        Self {
            playback_active: Some(
                meter
                    .i64_up_down_counter("remedia.playback.active_sessions")
                    .with_description("Number of active playback sessions")
                    .build(),
            ),
            playback_starts: Some(
                meter
                    .u64_counter("remedia.playback.starts")
                    .with_description("Playback sessions that started successfully")
                    .build(),
            ),
            playback_completions: Some(
                meter
                    .u64_counter("remedia.playback.completions")
                    .with_description("Playback sessions that reached natural end of stream")
                    .build(),
            ),
            playback_failures: Some(
                meter
                    .u64_counter("remedia.playback.failures")
                    .with_description("Playback sessions that failed")
                    .build(),
            ),
            decoder_startup_seconds: Some(
                meter
                    .f64_histogram("remedia.decoder.startup.duration")
                    .with_unit("s")
                    .with_description("Time spent opening and initializing a decoder")
                    .build(),
            ),
            voice_connections_active: Some(
                meter
                    .i64_up_down_counter("remedia.discord.voice.active_connections")
                    .with_description("Number of established Discord voice connections")
                    .build(),
            ),
            voice_connection_attempts: Some(
                meter
                    .u64_counter("remedia.discord.voice.connection_attempts")
                    .with_description("Discord voice connection attempts")
                    .build(),
            ),
            voice_reconnect_attempts: Some(
                meter
                    .u64_counter("remedia.discord.voice.reconnect_attempts")
                    .with_description("Discord voice Gateway resume attempts")
                    .build(),
            ),
            rtp_packets_sent: Some(
                meter
                    .u64_counter("remedia.discord.rtp.packets_sent")
                    .with_description("RTP audio packets sent over UDP")
                    .build(),
            ),
            rtp_packets_dropped: Some(
                meter
                    .u64_counter("remedia.discord.rtp.packets_dropped")
                    .with_description("RTP audio packets dropped before transmission")
                    .build(),
            ),
            packet_underruns: Some(
                meter
                    .u64_counter("remedia.discord.rtp.underruns")
                    .with_description("Scheduled RTP sends with no audio packet available")
                    .build(),
            ),
            connection_establishment_seconds: Some(
                meter
                    .f64_histogram("remedia.discord.voice.connection.duration")
                    .with_unit("s")
                    .with_description("Time taken to establish a Discord voice connection")
                    .build(),
            ),
            scheduling_delay_seconds: Some(
                meter
                    .f64_histogram("remedia.discord.rtp.scheduling_delay")
                    .with_unit("s")
                    .with_description("Delay between an RTP packet deadline and scheduling")
                    .build(),
            ),
        }
    }

    /// Record a successful playback start and increment the active-session gauge.
    pub fn playback_started(&self) {
        add_one(&self.playback_starts);
        add_active(&self.playback_active, 1);
    }

    /// Record natural end-of-stream completion.
    pub fn playback_completed(&self) {
        add_one(&self.playback_completions);
    }

    /// Record a playback failure.
    pub fn playback_failed(&self) {
        add_one(&self.playback_failures);
    }

    /// Decrement the active playback-session gauge after worker cleanup.
    pub fn playback_stopped(&self) {
        add_active(&self.playback_active, -1);
    }

    /// Record decoder-open duration in seconds.
    pub fn record_decoder_startup(&self, seconds: f64) {
        if let Some(histogram) = &self.decoder_startup_seconds {
            histogram.record(seconds, &[]);
        }
    }

    /// Record a voice connection attempt.
    pub fn voice_connection_attempt(&self) {
        add_one(&self.voice_connection_attempts);
    }

    /// Record voice connection establishment duration in seconds.
    pub fn voice_connection_established(&self, seconds: f64) {
        if let Some(histogram) = &self.connection_establishment_seconds {
            histogram.record(seconds, &[]);
        }
    }

    /// Increase or decrease the active voice connection gauge.
    pub fn voice_connection_active(&self, active: bool) {
        add_active(&self.voice_connections_active, if active { 1 } else { -1 });
    }

    /// Record a voice gateway reconnect attempt.
    pub fn voice_reconnect_attempt(&self) {
        add_one(&self.voice_reconnect_attempts);
    }

    /// Record an outbound RTP packet.
    pub fn rtp_packet_sent(&self) {
        add_one(&self.rtp_packets_sent);
    }

    /// Record one dropped RTP packet.
    pub fn rtp_packet_dropped(&self) {
        self.rtp_packet_dropped_by(1);
    }

    /// Record multiple RTP packets discarded from a bounded queue.
    pub fn rtp_packet_dropped_by(&self, count: u64) {
        if let Some(counter) = &self.rtp_packets_dropped {
            counter.add(count, &[]);
        }
    }

    /// Record an RTP schedule deadline with no media packet available.
    pub fn packet_underrun(&self) {
        add_one(&self.packet_underruns);
    }

    /// Record an RTP scheduling delay in seconds.
    pub fn record_scheduling_delay(&self, seconds: f64) {
        if let Some(histogram) = &self.scheduling_delay_seconds {
            histogram.record(seconds, &[]);
        }
    }

    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.playback_active.is_some()
    }
}

fn add_one(counter: &Option<Counter<u64>>) {
    if let Some(counter) = counter {
        counter.add(1, &[]);
    }
}

fn add_active(counter: &Option<UpDownCounter<i64>>, delta: i64) {
    if let Some(counter) = counter {
        counter.add(delta, &[]);
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::RemediaMetrics;

    #[test]
    fn disabled_metrics_are_noop_and_meter_records_lifecycle_accounting() {
        assert!(!RemediaMetrics::default().is_enabled());
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let metrics = RemediaMetrics::from_meter(provider.meter("remedia.test"));
        assert!(metrics.is_enabled());

        metrics.playback_started();
        metrics.playback_completed();
        metrics.playback_stopped();
        provider.force_flush().unwrap();

        let exported = exporter.get_finished_metrics().unwrap();
        let metrics: Vec<_> = exported
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .collect();
        let names: Vec<_> = metrics
            .iter()
            .map(|metric| metric.name().to_owned())
            .collect();
        assert!(names.iter().any(|name| name == "remedia.playback.starts"));
        assert!(
            names
                .iter()
                .any(|name| name == "remedia.playback.completions")
        );
        assert!(
            names
                .iter()
                .any(|name| name == "remedia.playback.active_sessions")
        );
        let start_metric = metrics
            .iter()
            .find(|metric| metric.name() == "remedia.playback.starts")
            .unwrap();
        match start_metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                assert_eq!(sum.data_points().next().unwrap().value(), 1);
            }
            data => panic!("unexpected starts aggregation: {data:?}"),
        }
        let active_metric = metrics
            .iter()
            .find(|metric| metric.name() == "remedia.playback.active_sessions")
            .unwrap();
        match active_metric.data() {
            AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                assert_eq!(sum.data_points().next().unwrap().value(), 0);
            }
            data => panic!("unexpected active-session aggregation: {data:?}"),
        }
        provider.shutdown().unwrap();
    }
}

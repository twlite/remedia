//! Controlled encoded-packet output for playback synchronization tests.

use super::*;

#[derive(Clone)]
pub(super) struct StepPacketSink(Arc<StepSinkShared>, bool);

#[derive(Default)]
pub(super) struct StepSinkShared {
    pub(super) data: Mutex<StepSinkData>,
    pub(super) changed: Condvar,
}

#[derive(Default)]
pub(super) struct StepSinkData {
    pub(super) permits: usize,
    pub(super) calls: usize,
    pub(super) packets: Vec<EncodedOpusPacket>,
    pub(super) flushes: usize,
    pub(super) finished: bool,
    pub(super) cancelled: bool,
    pub(super) filter_update_completions: usize,
    pub(super) last_filter_update_result: Option<Result<(), String>>,
}

#[derive(Clone)]
pub(super) struct StepPacketHandle(Arc<StepSinkShared>);

pub(super) enum StepSinkProgress {
    FilterUpdateCompleted(Result<(), String>),
    PacketBlocked(usize),
    Cancelled,
    TimedOut,
}

impl StepPacketSink {
    pub(super) fn new() -> (Self, StepPacketHandle) {
        Self::with_passthrough(true)
    }

    pub(super) fn without_passthrough() -> (Self, StepPacketHandle) {
        Self::with_passthrough(false)
    }

    pub(super) fn with_passthrough(passthrough_supported: bool) -> (Self, StepPacketHandle) {
        let shared = Arc::new(StepSinkShared::default());
        (
            Self(Arc::clone(&shared), passthrough_supported),
            StepPacketHandle(shared),
        )
    }
}

impl StepPacketHandle {
    pub(super) fn release(&self, count: usize) {
        let mut data = lock(&self.0.data);
        data.permits = data.permits.saturating_add(count);
        self.0.changed.notify_all();
    }

    pub(super) fn filter_update_completions(&self) -> usize {
        lock(&self.0.data).filter_update_completions
    }

    pub(super) fn record_filter_update(&self, result: Result<(), String>) {
        let mut data = lock(&self.0.data);
        data.filter_update_completions += 1;
        data.last_filter_update_result = Some(result);
        self.0.changed.notify_all();
    }

    pub(super) fn wait_for_update_or_blocked_packet(
        &self,
        after_calls: usize,
        after_updates: usize,
        timeout: Duration,
    ) -> StepSinkProgress {
        let data = lock(&self.0.data);
        let (data, wait_result) = self
            .0
            .changed
            .wait_timeout_while(data, timeout, |data| {
                data.filter_update_completions <= after_updates
                    && (data.calls <= after_calls
                        || data.calls <= data.packets.len()
                        || data.permits > 0)
                    && !data.cancelled
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if data.filter_update_completions > after_updates {
            return StepSinkProgress::FilterUpdateCompleted(
                data.last_filter_update_result
                    .clone()
                    .unwrap_or_else(|| Err("filter update completed without a result".into())),
            );
        }
        // send_packet increments calls before blocking and appends to
        // packets only after consuming a permit, so this identifies the
        // current blocked send without a separate notification channel.
        if data.calls > after_calls && data.calls > data.packets.len() && data.permits == 0 {
            return StepSinkProgress::PacketBlocked(data.calls);
        }
        if data.cancelled {
            return StepSinkProgress::Cancelled;
        }
        if wait_result.timed_out() {
            return StepSinkProgress::TimedOut;
        }
        StepSinkProgress::TimedOut
    }

    pub(super) fn transition_diagnostics(
        &self,
        session: &PlaybackSession,
        after_updates: usize,
    ) -> String {
        let (calls, completed_updates, update_result, cancelled) = {
            let data = lock(&self.0.data);
            (
                data.calls,
                data.filter_update_completions,
                data.last_filter_update_result.clone(),
                data.cancelled,
            )
        };
        let update_status = if completed_updates > after_updates {
            format!("completed: {update_result:?}")
        } else if cancelled {
            "pending; sink cancelled".to_owned()
        } else {
            "pending".to_owned()
        };
        format!(
            "playback_state={:?}, processing_mode={:?}, sink_call_count={calls}, filter_update={update_status}",
            session.state(),
            session.processing_mode()
        )
    }

    pub(super) fn wait_for_calls(&self, count: usize) -> bool {
        let data = lock(&self.0.data);
        let (data, _) = self
            .0
            .changed
            .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                data.calls < count && !data.cancelled
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.calls >= count
    }

    pub(super) fn wait_for_pending_filter(&self, session: &PlaybackSession) -> bool {
        let data = lock(&session.shared.data);
        let (data, _) = session
            .shared
            .changed
            .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                data.pending_filter_graph.is_none()
                    && !matches!(data.state, PlaybackState::Stopped | PlaybackState::Failed)
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.pending_filter_graph.is_some()
    }

    pub(super) fn wait_for_pending_seek(&self, session: &PlaybackSession) -> bool {
        let data = lock(&session.shared.data);
        let (data, _) = session
            .shared
            .changed
            .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                data.pending_seek.is_none()
                    && !matches!(data.state, PlaybackState::Stopped | PlaybackState::Failed)
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.pending_seek.is_some()
    }

    pub(super) fn snapshot(&self) -> StepSinkData {
        let data = lock(&self.0.data);
        StepSinkData {
            permits: data.permits,
            calls: data.calls,
            packets: data.packets.clone(),
            flushes: data.flushes,
            finished: data.finished,
            cancelled: data.cancelled,
            filter_update_completions: data.filter_update_completions,
            last_filter_update_result: data.last_filter_update_result.clone(),
        }
    }
}

impl OpusPacketSink for StepPacketSink {
    fn supports_passthrough_packet(&self, _packet: &EncodedOpusPacket) -> bool {
        self.1
    }

    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let mut data = lock(&self.0.data);
        data.calls += 1;
        self.0.changed.notify_all();
        while data.permits == 0 && !data.cancelled && !cancellation.is_cancelled() {
            data = self
                .0
                .changed
                .wait(data)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if data.cancelled || cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        data.permits -= 1;
        data.packets.push(packet);
        self.0.changed.notify_all();
        Ok(())
    }

    fn flush(
        &self,
        _position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        lock(&self.0.data).flushes += 1;
        Ok(())
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        lock(&self.0.data).finished = true;
        Ok(())
    }

    fn cancel(&self) {
        let mut data = lock(&self.0.data);
        data.cancelled = true;
        self.0.changed.notify_all();
    }
}

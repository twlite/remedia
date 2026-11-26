use super::*;
use std::task::{Context, Poll};

impl RoutedOpusSender {
    pub(super) fn poll_target(
        &self,
        cx: &mut Context<'_>,
        cancellation: &CancellationToken,
        mut action: impl FnMut(&DiscordOpusSender, &mut Context<'_>) -> Poll<Result<(), OutputError>>,
    ) -> Poll<Result<(), OutputError>> {
        self.router.inner.readiness.register(cx.waker());
        let (generation, target) = {
            let state = lock(&self.router.inner.state);
            if self.cancelled.load(Ordering::Acquire)
                || cancellation.is_cancelled()
                || state.active_playback_generation != Some(self.playback_generation)
            {
                return Poll::Ready(Err(OutputError::Cancelled));
            }
            if let Some(message) = &state.failure {
                return Poll::Ready(Err(OutputError::new(message.clone())));
            }
            let Some(target) = state.target.as_ref() else {
                return Poll::Pending;
            };
            (state.transport_generation, target.clone())
        };
        match action(&target, cx) {
            Poll::Ready(Err(error)) => {
                let mut state = lock(&self.router.inner.state);
                let replaced = state.transport_generation != generation;
                let failed = matches!(
                    target.state(),
                    ConnectionState::Disconnected | ConnectionState::Failed
                );
                if !replaced && failed {
                    state.transport_generation = state.transport_generation.wrapping_add(1);
                    state.target = None;
                    state.transport_state = RouterTransportState::Reconnecting;
                }
                if replaced {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else if failed {
                    Poll::Pending
                } else {
                    Poll::Ready(Err(error))
                }
            }
            Poll::Ready(Ok(())) => {
                if lock(&self.router.inner.state).active_playback_generation
                    != Some(self.playback_generation)
                {
                    Poll::Ready(Err(OutputError::Cancelled))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

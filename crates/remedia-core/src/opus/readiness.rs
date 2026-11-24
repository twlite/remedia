use super::*;
use crate::OutputControl;
use std::task::{Context, Poll, ready};

impl OpusEncoderOutput {
    pub(super) fn poll_production(
        &self,
        cx: &mut Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        self.ensure_active(cancellation)?;
        ready!(self.sink.poll(cx, cancellation))?;
        let mut pending = lock(&self.pending_pcm);
        let Some((frame, offset)) = pending.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        let end = (*offset + OPUS_FRAME_SCALARS).min(frame.samples.len());
        let mut state = lock(&self.state);
        self.initialize_encoder(&mut state)?;
        Self::write_pcm(
            &mut state,
            &frame.samples[*offset..end],
            frame.start_sample + (*offset / 2) as u64,
            &self.sink,
            cancellation,
        )?;
        *offset = end;
        let finished = end == frame.samples.len();
        if finished {
            pending.take();
        }
        drop(state);
        drop(pending);
        ready!(self.sink.poll(cx, cancellation))?;
        if finished {
            Poll::Ready(Ok(()))
        } else {
            // Yield after one encoder frame, preserving the owned PCM allocation.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    pub(super) fn poll_barrier(
        &self,
        control: OutputControl,
        cx: &mut Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        ready!(self.poll_production(cx, cancellation))?;
        if control == OutputControl::Finish && !lock(&self.state).finished {
            self.finish(cancellation)?;
            ready!(self.sink.poll(cx, cancellation))?;
        }
        self.sink
            .inner
            .scheduler_poll_control(control, cx, cancellation)
    }
}

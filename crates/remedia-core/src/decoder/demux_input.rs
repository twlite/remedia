//! Exclusive demux ownership. Only raw AVPackets cross the I/O/codec boundary.
use super::*;
use crate::input::OpenedInput;
#[cfg(feature = "experimental-scheduler")]
use std::sync::Arc;
use std::task::{Context, Poll};

pub(super) struct DemuxInput {
    direct: Option<OpenedInput>,
    #[cfg(feature = "experimental-scheduler")]
    name: String,
    network: bool,
    cancellation: CancellationToken,
    #[cfg(feature = "experimental-scheduler")]
    queued: Option<Arc<queued::QueuedInput>>,
}

impl DemuxInput {
    pub(super) fn new(input: OpenedInput, network: bool, cancellation: CancellationToken) -> Self {
        Self {
            #[cfg(feature = "experimental-scheduler")]
            name: input.format().name().to_owned(),
            direct: Some(input),
            network,
            cancellation,
            #[cfg(feature = "experimental-scheduler")]
            queued: None,
        }
    }

    #[cfg(feature = "experimental-scheduler")]
    pub(super) fn name(&self) -> &str {
        &self.name
    }

    #[cfg(feature = "experimental-scheduler")]
    pub(crate) fn isolate(
        &mut self,
        executor: Arc<crate::playback::blocking::BlockingExecutor>,
    ) -> Option<Arc<queued::QueuedInput>> {
        if self.queued.is_none() {
            self.queued = Some(queued::QueuedInput::new(
                self.direct.take().expect("direct demux input"),
                executor,
                self.network,
                self.cancellation.clone(),
            ));
        }
        self.queued.clone()
    }

    pub(super) fn supports_random_access(&self) -> bool {
        #[cfg(feature = "experimental-scheduler")]
        if let Some(input) = &self.queued {
            return input.supports_random_access();
        }
        self.direct
            .as_ref()
            .expect("direct input")
            .supports_random_access()
    }

    pub(super) fn seek(&mut self, timestamp: i64) -> Result<(), ffmpeg::Error> {
        #[cfg(feature = "experimental-scheduler")]
        if let Some(input) = &self.queued {
            return input.seek(timestamp);
        }
        self.direct
            .as_mut()
            .expect("direct input")
            .seek(timestamp, ..timestamp)
    }

    pub(super) fn poll_packet(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<ffmpeg::Packet>, DecoderError>> {
        #[cfg(feature = "experimental-scheduler")]
        if let Some(input) = &self.queued {
            return input.poll_packet(cx);
        }
        let _ = cx;
        Poll::Ready(read_packet(
            self.direct.as_mut().expect("direct input"),
            self.network,
            &self.cancellation,
        ))
    }

    pub(super) fn wait_ready(&self) {
        #[cfg(feature = "experimental-scheduler")]
        if let Some(input) = &self.queued {
            input.wait_ready();
        }
    }
}

fn read_packet(
    input: &mut OpenedInput,
    network: bool,
    cancellation: &CancellationToken,
) -> Result<Option<ffmpeg::Packet>, DecoderError> {
    loop {
        if cancellation.is_cancelled() {
            return Err(DecoderError::Cancelled);
        }
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(input) {
            Ok(()) => return Ok(Some(packet)),
            Err(ffmpeg::Error::Eof) => {
                return match input.producer_failure() {
                    Some(message) => Err(DecoderError::InputFailure(message)),
                    None => Ok(None),
                };
            }
            Err(error) if is_again(error) => continue,
            Err(error) => {
                return Err(match input.producer_failure() {
                    Some(message) => DecoderError::InputFailure(message),
                    None => ffmpeg_error(
                        error,
                        "reading media packets",
                        cancellation.is_cancelled(),
                        network,
                    ),
                });
            }
        }
    }
}

#[cfg(feature = "experimental-scheduler")]
pub(crate) mod queued;

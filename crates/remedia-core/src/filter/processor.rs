use super::*;

pub(crate) struct PcmProcessor {
    pub(super) graph: Option<PcmFilterGraph>,
    pub(super) next_sample: u64,
    pub(super) source_position: Option<SourcePosition>,
}

impl PcmProcessor {
    pub(crate) fn new(
        expression: Option<&str>,
        start_sample: u64,
    ) -> Result<Self, FilterGraphError> {
        Ok(Self {
            graph: expression.map(PcmFilterGraph::build).transpose()?,
            next_sample: start_sample,
            source_position: None,
        })
    }

    pub(crate) fn next_sample(&self) -> u64 {
        self.next_sample
    }

    pub(crate) fn source_position(&self) -> Option<SourcePosition> {
        self.source_position
    }

    /// Advance the normalized output timeline for an encoded Opus packet that
    /// bypassed PCM processing. RTP keeps its own packet-duration clock.
    pub(crate) fn record_passthrough_packet(
        &mut self,
        samples_per_channel: u32,
        source_start_sample: Option<u64>,
        source_position_estimated: bool,
    ) -> Result<u64, FilterGraphError> {
        if self.graph.is_some() {
            return Err(FilterGraphError::Processing(
                "cannot advance encoded passthrough while a filtergraph is active".into(),
            ));
        }
        let start_sample = self.next_sample;
        self.next_sample = self
            .next_sample
            .checked_add(u64::from(samples_per_channel))
            .ok_or_else(|| {
                FilterGraphError::Processing("output sample timeline overflowed".into())
            })?;
        if let Some(source_start_sample) = source_start_sample {
            self.source_position = Some(SourcePosition {
                sample: source_start_sample.saturating_add(u64::from(samples_per_channel)),
                estimated: source_position_estimated,
            });
        }
        Ok(start_sample)
    }

    pub(crate) fn expression(&self) -> Option<&str> {
        self.graph.as_ref().map(PcmFilterGraph::expression)
    }

    pub(crate) fn replace(&mut self, graph: Option<PcmFilterGraph>) {
        self.graph = graph;
    }

    pub(crate) fn reset(&mut self, start_sample: u64) -> Result<(), FilterGraphError> {
        let expression = self.expression().map(str::to_owned);
        self.graph = expression
            .as_deref()
            .map(PcmFilterGraph::build)
            .transpose()?;
        self.next_sample = start_sample;
        self.source_position = Some(SourcePosition {
            sample: start_sample,
            estimated: true,
        });
        Ok(())
    }

    pub(crate) fn write_frame(
        &mut self,
        input: PcmFrame,
        source_start_sample: Option<u64>,
        source_position_estimated: bool,
        gain: f32,
        mut deliver: impl FnMut(PcmFrame, Option<SourcePosition>) -> Result<(), OutputError>,
    ) -> Result<(), ProcessError> {
        let source_position_estimated = source_position_estimated || self.graph.is_some();
        let source_position = source_start_sample.map(|sample| SourcePosition {
            sample: sample.saturating_add(input.samples_per_channel as u64),
            estimated: source_position_estimated,
        });
        if source_position.is_some() {
            self.source_position = source_position;
        }
        if self.graph.is_some() {
            self.graph
                .as_mut()
                .expect("graph existence checked")
                .push(&input)
                .map_err(ProcessError::Filter)?;
            loop {
                let frame = self
                    .graph
                    .as_mut()
                    .expect("graph existence checked")
                    .pull()
                    .map_err(ProcessError::Filter)?;
                match frame {
                    Some(frame) => {
                        self.deliver(frame, gain, true, source_position, &mut deliver)?
                    }
                    None => break,
                }
            }
        } else {
            self.deliver(input, gain, false, source_position, &mut deliver)?;
        }
        Ok(())
    }

    pub(crate) fn finish(
        &mut self,
        gain: f32,
        mut deliver: impl FnMut(PcmFrame, Option<SourcePosition>) -> Result<(), OutputError>,
    ) -> Result<(), ProcessError> {
        if self.graph.is_none() {
            return Ok(());
        }
        self.graph
            .as_mut()
            .expect("graph existence checked")
            .signal_eof()
            .map_err(ProcessError::Filter)?;
        loop {
            let frame = self
                .graph
                .as_mut()
                .expect("graph existence checked")
                .pull()
                .map_err(ProcessError::Filter)?;
            match frame {
                Some(frame) => {
                    self.deliver(frame, gain, true, self.source_position, &mut deliver)?
                }
                None => return Ok(()),
            }
        }
    }

    pub(super) fn deliver(
        &mut self,
        mut frame: PcmFrame,
        gain: f32,
        clip_output: bool,
        source_position: Option<SourcePosition>,
        deliver: &mut impl FnMut(PcmFrame, Option<SourcePosition>) -> Result<(), OutputError>,
    ) -> Result<(), ProcessError> {
        let scalar_count = frame.samples_per_channel.checked_mul(2).ok_or_else(|| {
            ProcessError::Filter(FilterGraphError::Processing(
                "filtered PCM sample count overflowed".into(),
            ))
        })?;
        if scalar_count != frame.samples.len() {
            return Err(ProcessError::Filter(FilterGraphError::Processing(
                "FFmpeg returned PCM that is not interleaved stereo".into(),
            )));
        }
        if gain != 1.0 || clip_output {
            for sample in &mut frame.samples {
                let adjusted = *sample * gain;
                *sample = if adjusted.is_finite() {
                    adjusted.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
            }
        }
        frame.start_sample = self.next_sample;
        self.next_sample = self
            .next_sample
            .saturating_add(u64::try_from(frame.samples_per_channel).unwrap_or(u64::MAX));
        deliver(frame, source_position).map_err(ProcessError::Output)
    }
}

#[derive(Debug)]
pub(crate) enum ProcessError {
    Filter(FilterGraphError),
    Output(OutputError),
}

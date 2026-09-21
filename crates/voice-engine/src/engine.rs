use std::time::Instant;

use tracing::warn;
use voice_codec::packet::f32_to_i16;
use voice_codec::{MimiCodec, CODEBOOKS};
use voice_rt::noalloc::permit_alloc;

use crate::metrics::Metrics;

pub enum StepOutcome {
    /// `output` holds codes to synthesize.
    Speak,
    /// Nothing to say this frame; the server emits silence.
    Silent,
}

/// The model seam: one call per 80 ms frame, Mimi codes in, Mimi codes out.
pub trait Engine: Send {
    fn step(&mut self, input: &[u32; CODEBOOKS], output: &mut [u32; CODEBOOKS]) -> StepOutcome;
    fn reset(&mut self) {}
}

/// Echoes the user's codes back, which exercises both codec directions end to end.
pub struct LoopbackEngine;

impl Engine for LoopbackEngine {
    fn step(&mut self, input: &[u32; CODEBOOKS], output: &mut [u32; CODEBOOKS]) -> StepOutcome {
        *output = *input;
        StepOutcome::Speak
    }
}

/// A frame could not be processed. Carries no payload so that creating and dropping it inside a
/// `no_alloc` section is safe; implementations log the cause themselves.
#[derive(Debug, Clone, Copy)]
pub struct ProcessError;

/// Turns one input frame of PCM into one output frame. Runs on the model thread inside a
/// `no_alloc` section.
pub trait FrameProcessor: Send {
    fn process(&mut self, input: &[i16], output: &mut [i16], metrics: &Metrics) -> Result<(), ProcessError>;
    fn reset(&mut self);
}

/// Copies input to output. Used to test the transport path without model weights.
pub struct Passthrough;

impl FrameProcessor for Passthrough {
    fn process(&mut self, input: &[i16], output: &mut [i16], _metrics: &Metrics) -> Result<(), ProcessError> {
        output.copy_from_slice(input);
        Ok(())
    }

    fn reset(&mut self) {}
}

pub struct MimiProcessor<E> {
    codec: MimiCodec,
    engine: E,
}

impl<E: Engine> MimiProcessor<E> {
    pub fn new(codec: MimiCodec, engine: E) -> Self {
        Self { codec, engine }
    }
}

impl<E: Engine> FrameProcessor for MimiProcessor<E> {
    fn process(&mut self, input: &[i16], output: &mut [i16], metrics: &Metrics) -> Result<(), ProcessError> {
        output.fill(0);

        let start = Instant::now();
        // The anyhow error is logged and dropped while allocation is still permitted.
        let codes = permit_alloc(|| self.codec.encode(input).map_err(|error| warn!(%error, "mimi encode failed")));
        metrics.mimi_encode.record(start.elapsed());
        let Some(codes) = codes.map_err(|()| ProcessError)? else { return Ok(()) };

        let start = Instant::now();
        let mut reply = [0u32; CODEBOOKS];
        let outcome = self.engine.step(&codes, &mut reply);
        metrics.engine_step.record(start.elapsed());
        if matches!(outcome, StepOutcome::Silent) {
            return Ok(());
        }

        let start = Instant::now();
        let decoded = permit_alloc(|| match self.codec.decode(&reply) {
            Ok(pcm) => Ok(pcm.map(|pcm| f32_to_i16(pcm, output))),
            Err(error) => {
                warn!(%error, "mimi decode failed");
                Err(ProcessError)
            }
        });
        metrics.mimi_decode.record(start.elapsed());
        decoded.map(|_| ())
    }

    fn reset(&mut self) {
        self.codec.reset();
        self.engine.reset();
    }
}

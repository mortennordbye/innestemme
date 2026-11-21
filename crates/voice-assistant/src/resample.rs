//! Converts whole utterances from another speech engine's rate (Piper: 22.05 or 16 kHz) to the
//! engine's 24 kHz. Windowed sinc, so upsampling does not add the aliasing a linear interpolation would.
//! Also the engine's 24 kHz to the 16 kHz the speech-to-text models take, and model output to PCM.

use voice_proto::SAMPLE_RATE;

/// Taps on each side of the output point.
const HALF_TAPS: i64 = 16;

pub fn to_24k(pcm: &[i16], rate: u32) -> Vec<i16> {
    if rate == SAMPLE_RATE || pcm.is_empty() {
        return pcm.to_vec();
    }
    let step = rate as f64 / SAMPLE_RATE as f64;
    // Cut at the lower Nyquist frequency, a little below it for the window's transition band.
    let cutoff = (1.0f64).min(1.0 / step) * 0.95;
    let out_len = ((pcm.len() as f64) / step).floor() as usize;
    (0..out_len)
        .map(|i| {
            let t = i as f64 * step;
            let centre = t.floor() as i64;
            let mut acc = 0.0;
            for k in (centre - HALF_TAPS + 1)..=(centre + HALF_TAPS) {
                let Some(&s) = usize::try_from(k).ok().and_then(|k| pcm.get(k)) else {
                    continue;
                };
                let x = t - k as f64;
                let window = 0.5 + 0.5 * (std::f64::consts::PI * x / HALF_TAPS as f64).cos();
                acc += s as f64 * cutoff * sinc(cutoff * x) * window;
            }
            acc.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
        })
        .collect()
}

/// The same filter for a stream arriving in chunks (satellite microphones): keeps the input tail
/// the next chunk needs, so chunk edges add no clicks. Adds `HALF_TAPS` input samples of delay.
pub struct Stream {
    step: f64,
    cutoff: f64,
    buf: Vec<i16>,
    /// Input index of `buf[0]`.
    base: i64,
    /// Input position of the next output sample.
    next: f64,
}

impl Stream {
    pub fn new(from: u32, to: u32) -> Self {
        let step = from as f64 / to as f64;
        Self { step, cutoff: (1.0f64).min(1.0 / step) * 0.95, buf: Vec::new(), base: 0, next: 0.0 }
    }

    pub fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        self.buf.extend_from_slice(input);
        let end = self.base + self.buf.len() as i64;
        while (self.next.floor() as i64) + HALF_TAPS < end {
            let centre = self.next.floor() as i64;
            let mut acc = 0.0;
            for k in (centre - HALF_TAPS + 1)..=(centre + HALF_TAPS) {
                let Some(&s) = usize::try_from(k - self.base).ok().and_then(|i| self.buf.get(i)) else {
                    continue;
                };
                let x = self.next - k as f64;
                let window = 0.5 + 0.5 * (std::f64::consts::PI * x / HALF_TAPS as f64).cos();
                acc += s as f64 * self.cutoff * sinc(self.cutoff * x) * window;
            }
            out.push(acc.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            self.next += self.step;
        }
        let keep_from = (self.next.floor() as i64 - HALF_TAPS).max(self.base);
        let drop = (keep_from - self.base) as usize;
        if drop > 0 {
            self.buf.drain(..drop);
            self.base = keep_from;
        }
    }
}

/// 24 kHz -> 16 kHz (up 2, low-pass at 8 kHz, down 3), output scaled to [-1, 1].
pub fn resample_24k_to_16k(pcm: &[i16]) -> Vec<f32> {
    const TAPS: usize = 96;
    // Cutoff 7.2 kHz at the 48 kHz intermediate rate, Hann window, gain 2 for the zero stuffing.
    let h: Vec<f64> = (0..TAPS)
        .map(|i| {
            let x = i as f64 - (TAPS - 1) as f64 / 2.0;
            let fc = 7200.0 / 48000.0;
            let sinc =
                if x == 0.0 { 2.0 * fc } else { (std::f64::consts::TAU * fc * x).sin() / (std::f64::consts::PI * x) };
            let hann = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (TAPS - 1) as f64).cos();
            2.0 * sinc * hann
        })
        .collect();
    let out_len = pcm.len() * 2 / 3;
    (0..out_len)
        .map(|n| {
            // Upsampled index 3n + delay; only even upsampled indices carry input samples.
            let center = 3 * n + TAPS / 2;
            let mut acc = 0.0;
            for (k, tap) in h.iter().enumerate() {
                let Some(up) = center.checked_sub(k) else {
                    break;
                };
                if up % 2 == 0 {
                    if let Some(&s) = pcm.get(up / 2) {
                        acc += tap * f64::from(s);
                    }
                }
            }
            (acc / 32768.0) as f32
        })
        .collect()
}

/// A model's sample in [-1, 1] as 16-bit PCM; louder samples are clipped.
pub fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_in_chunks_matches_the_whole_buffer() {
        let tone: Vec<i16> = (0..16_000)
            .map(|i| ((i as f64 / 16_000.0 * 440.0 * std::f64::consts::TAU).sin() * 10_000.0) as i16)
            .collect();
        let mut stream = Stream::new(16_000, 24_000);
        let mut out = Vec::new();
        for chunk in tone.chunks(333) {
            stream.push(chunk, &mut out);
        }
        let whole = Stream::new(16_000, 24_000);
        let mut expected = Vec::new();
        let mut whole = whole;
        whole.push(&tone, &mut expected);
        assert_eq!(out, expected);
        // Everything but the filter's look-ahead comes out: 1.5 samples per input sample.
        assert!((23_970..=24_000).contains(&out.len()), "{}", out.len());
        let peak = out[1000..23_000].iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((9_800..=10_200).contains(&peak), "peak {peak}");
    }

    #[test]
    fn keeps_a_tone_and_the_duration() {
        let rate = 22_050;
        let tone: Vec<i16> = (0..rate)
            .map(|i| ((i as f64 / rate as f64 * 440.0 * std::f64::consts::TAU).sin() * 10_000.0) as i16)
            .collect();
        let out = to_24k(&tone, rate as u32);
        assert_eq!(out.len(), 24_000);
        // Away from the edges the amplitude stays within 2% of the input's.
        let peak = out[1000..23_000].iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((9_800..=10_200).contains(&peak), "peak {peak}");
        // And the zero crossings match 440 Hz.
        let crossings = out.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        assert!((878..=882).contains(&crossings), "crossings {crossings}");
    }

    #[test]
    fn resampler_keeps_a_1khz_tone() {
        let pcm: Vec<i16> = (0..24_000)
            .map(|i| ((i as f64 / 24_000.0 * 1000.0 * std::f64::consts::TAU).sin() * 10_000.0) as i16)
            .collect();
        let out = resample_24k_to_16k(&pcm);
        assert_eq!(out.len(), 16_000);
        let rms = (out[1000..15_000].iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / 14_000.0).sqrt();
        assert!((rms - 10_000.0 / 32768.0 / 2f64.sqrt()).abs() < 0.01, "rms {rms}");
        // Phase: 16 kHz sample n should match the 1 kHz sine at t = n / 16000.
        let expected = (1000.0 / 16_000.0 * 1000.0 * std::f64::consts::TAU).sin() * 10_000.0 / 32768.0;
        assert!((f64::from(out[1000]) - expected).abs() < 0.02, "{} vs {expected}", out[1000]);
    }
}

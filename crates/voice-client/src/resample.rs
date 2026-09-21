//! Fixed 2:1 rate conversion for devices that only run at 48 kHz.

const TAPS: usize = 31;

/// Windowed-sinc low-pass at a quarter of the high rate (the Nyquist of the low rate).
fn taps() -> [f32; TAPS] {
    let mid = (TAPS / 2) as f32;
    let mut h = [0f32; TAPS];
    for (i, tap) in h.iter_mut().enumerate() {
        let x = i as f32 - mid;
        let sinc = if x == 0.0 { 0.5 } else { (std::f32::consts::PI * 0.5 * x).sin() / (std::f32::consts::PI * x) };
        let hann = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (TAPS - 1) as f32).cos();
        *tap = sinc * hann;
    }
    let sum: f32 = h.iter().sum();
    h.map(|t| t / sum)
}

pub struct Fir2 {
    taps: [f32; TAPS],
    history: [f32; TAPS],
    pos: usize,
    phase: bool,
}

impl Fir2 {
    pub fn new() -> Self {
        Self { taps: taps(), history: [0.0; TAPS], pos: 0, phase: false }
    }

    fn push(&mut self, x: f32) -> f32 {
        self.history[self.pos] = x;
        self.pos = (self.pos + 1) % TAPS;
        (0..TAPS).map(|k| self.taps[k] * self.history[(self.pos + TAPS - 1 - k) % TAPS]).sum()
    }

    /// 48 kHz in, 24 kHz out: returns a sample for every second input.
    pub fn decimate(&mut self, x: f32) -> Option<f32> {
        let y = self.push(x);
        self.phase = !self.phase;
        self.phase.then_some(y)
    }

    /// 24 kHz in, 48 kHz out: two samples per input.
    pub fn interpolate(&mut self, x: f32) -> [f32; 2] {
        [self.push(x * 2.0), self.push(0.0)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn passes_speech_band_and_rejects_aliases() {
        let tone = |hz: f32, rate: f32, n: usize| (0..n).map(move |i| (i as f32 / rate * hz * std::f32::consts::TAU).sin());
        let mut fir = Fir2::new();
        let low: Vec<f32> = tone(1_000.0, 48_000.0, 9_600).filter_map(|s| fir.decimate(s)).collect();
        assert!((rms(&low[100..]) - 0.707).abs() < 0.03);
        let mut fir = Fir2::new();
        let alias: Vec<f32> = tone(20_000.0, 48_000.0, 9_600).filter_map(|s| fir.decimate(s)).collect();
        assert!(rms(&alias[100..]) < 0.02);
        let mut fir = Fir2::new();
        let up: Vec<f32> = tone(1_000.0, 24_000.0, 4_800).flat_map(|s| fir.interpolate(s)).collect();
        assert!((rms(&up[100..]) - 0.707).abs() < 0.03);
    }
}

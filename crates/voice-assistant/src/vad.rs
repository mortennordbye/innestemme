//! Energy-based utterance detection on 80 ms frames. Enough for a quiet room; the noise floor
//! adapts slowly so a fan or the fridge does not count as speech.

use std::collections::VecDeque;

use voice_proto::FRAME_SAMPLES;

/// Speech starts this far above the noise floor, for `START_FRAMES` frames in a row.
const START_DB: f32 = 12.0;
const START_FRAMES: u32 = 2;
/// And ends after `END_FRAMES` frames that are either close to the floor (`END_DB`) or well
/// below the loudest frame of the utterance (`END_BELOW_PEAK_DB`), whichever threshold is higher.
/// The second one ends an utterance when background talk or noise never lets the room go quiet.
const END_DB: f32 = 8.0;
const END_BELOW_PEAK_DB: f32 = 20.0;
const END_FRAMES: u32 = 8;
/// Absolute minimum level for speech, for a digitally silent input.
const MIN_SPEECH_DB: f32 = -50.0;
const PREROLL_FRAMES: usize = 3;
/// Utterances with fewer loud frames than this are clicks or bumps.
const MIN_VOICED_FRAMES: u32 = 4;
/// 10 s.
const MAX_FRAMES: usize = 125;

pub struct Vad {
    floor: f32,
    loud_run: u32,
    quiet_run: u32,
    voiced: u32,
    speaking: bool,
    /// Loudest frame of the current utterance, in dBFS.
    peak: f32,
    preroll: VecDeque<Vec<i16>>,
    utterance: Vec<i16>,
}

impl Default for Vad {
    fn default() -> Self {
        Self {
            floor: -60.0,
            loud_run: 0,
            quiet_run: 0,
            voiced: 0,
            speaking: false,
            peak: -100.0,
            preroll: VecDeque::new(),
            utterance: Vec::new(),
        }
    }
}

pub fn level_db(frame: &[i16]) -> f32 {
    let energy = frame.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / frame.len().max(1) as f64;
    (10.0 * (energy / (32768.0f64 * 32768.0)).max(1e-10).log10()) as f32
}

impl Vad {
    pub fn is_speaking(&self) -> bool {
        self.speaking
    }

    pub fn reset(&mut self) {
        let floor = self.floor;
        *self = Self { floor, ..Self::default() };
    }

    /// Returns the finished utterance, with a little audio from before it started.
    pub fn push(&mut self, frame: &[i16]) -> Option<Vec<i16>> {
        debug_assert_eq!(frame.len(), FRAME_SAMPLES);
        let db = level_db(frame);
        let loud = db > (self.floor + START_DB).max(MIN_SPEECH_DB);
        if !self.speaking {
            // Falls fast, rises slowly: speech does not drag the floor up, silence pulls it down.
            self.floor = if db < self.floor { db } else { self.floor + 0.02 * (db - self.floor) }.max(-90.0);
            self.loud_run = if loud { self.loud_run + 1 } else { 0 };
            self.preroll.push_back(frame.to_vec());
            if self.preroll.len() > PREROLL_FRAMES + START_FRAMES as usize {
                self.preroll.pop_front();
            }
            if self.loud_run >= START_FRAMES {
                self.speaking = true;
                self.peak = db;
                self.voiced = self.loud_run;
                self.quiet_run = 0;
                self.utterance = self.preroll.drain(..).flatten().collect();
            }
            return None;
        }
        self.utterance.extend_from_slice(frame);
        if loud {
            self.voiced += 1;
        }
        self.peak = self.peak.max(db);
        let end_level = (self.floor + END_DB).max(self.peak - END_BELOW_PEAK_DB);
        self.quiet_run = if db < end_level { self.quiet_run + 1 } else { 0 };
        let too_long = self.utterance.len() >= MAX_FRAMES * FRAME_SAMPLES;
        if self.quiet_run < END_FRAMES && !too_long {
            return None;
        }
        self.speaking = false;
        self.loud_run = 0;
        let utterance = std::mem::take(&mut self.utterance);
        (self.voiced >= MIN_VOICED_FRAMES).then_some(utterance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(amplitude: f32) -> Vec<i16> {
        (0..FRAME_SAMPLES).map(|i| ((i as f32 * 0.07).sin() * amplitude) as i16).collect()
    }

    #[test]
    fn finds_one_utterance_between_silences() {
        let mut vad = Vad::default();
        let quiet = frame(30.0);
        let speech = frame(8000.0);
        let mut found = Vec::new();
        for f in std::iter::repeat_n(&quiet, 20)
            .chain(std::iter::repeat_n(&speech, 15))
            .chain(std::iter::repeat_n(&quiet, 20))
        {
            found.extend(vad.push(f));
        }
        assert_eq!(found.len(), 1);
        let frames = found[0].len() / FRAME_SAMPLES;
        // Speech, pre-roll and the closing silence.
        assert!(
            (15 + END_FRAMES as usize..=15 + END_FRAMES as usize + PREROLL_FRAMES + 1).contains(&frames),
            "{frames}"
        );
    }

    #[test]
    fn ends_over_quieter_background_talk() {
        let mut vad = Vad::default();
        let quiet = frame(30.0);
        let background = frame(600.0);
        let speech = frame(12000.0);
        let mut found = Vec::new();
        let input = std::iter::repeat_n(&quiet, 20)
            .chain(std::iter::repeat_n(&speech, 15))
            .chain(std::iter::repeat_n(&background, 40));
        for f in input {
            found.extend(vad.push(f));
        }
        assert_eq!(found.len(), 1);
        assert!(found[0].len() / FRAME_SAMPLES < 30);
    }

    #[test]
    fn ignores_a_click() {
        let mut vad = Vad::default();
        let quiet = frame(30.0);
        let mut found = Vec::new();
        for f in std::iter::repeat_n(&quiet, 20)
            .chain([&frame(8000.0), &frame(8000.0)])
            .chain(std::iter::repeat_n(&quiet, 20))
        {
            found.extend(vad.push(f));
        }
        assert!(found.is_empty());
    }
}

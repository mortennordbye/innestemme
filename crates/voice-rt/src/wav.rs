//! Mono 16-bit wav files, the only kind the tools and the engine read and write.

use std::path::Path;

use anyhow::{bail, Context, Result};

/// The samples of a mono 16-bit wav at `rate` Hz. Any other format is an error, not a silent
/// resample: a wrong rate would only show as a bad result much later.
pub fn read_mono(path: impl AsRef<Path>, rate: u32) -> Result<Vec<i16>> {
    let path = path.as_ref();
    let mut reader = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != rate
        || spec.channels != 1
        || spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
    {
        bail!("{} must be {} kHz mono 16-bit, got {spec:?}", path.display(), f64::from(rate) / 1000.0);
    }
    reader.samples::<i16>().collect::<Result<_, _>>().with_context(|| format!("reading {}", path.display()))
}

/// Writes `pcm` as a mono 16-bit wav at `rate` Hz.
pub fn write_mono(path: impl AsRef<Path>, rate: u32, pcm: &[i16]) -> Result<()> {
    let path = path.as_ref();
    let spec =
        hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut writer = hound::WavWriter::create(path, spec).with_context(|| format!("creating {}", path.display()))?;
    pcm.iter().try_for_each(|&s| writer.write_sample(s))?;
    writer.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_back_what_it_wrote_and_checks_the_rate() {
        let path = std::env::temp_dir().join(format!("voice-rt-wav-{}.wav", std::process::id()));
        let pcm = [0i16, 1, -1, i16::MAX, i16::MIN];
        write_mono(&path, 16_000, &pcm).unwrap();
        assert_eq!(read_mono(&path, 16_000).unwrap(), pcm);
        let error = read_mono(&path, 24_000).unwrap_err().to_string();
        assert!(error.contains("must be 24 kHz mono 16-bit"), "{error}");
        std::fs::remove_file(&path).ok();
    }
}

//! Speaks one sentence through a Wyoming (Piper) server into a 24 kHz wav, the audio the engine would
//! play: `cargo run -p voice-assistant --example speak -- 127.0.0.1:10200 no_NO-talesyntese-medium out.wav "Hei"`.

use voice_assistant::lang::Lang;
use voice_assistant::tts::Tts;
use voice_assistant::wyoming::WyomingTts;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [address, voice, out, text @ ..] = args.as_slice() else {
        anyhow::bail!("usage: speak <host:port> <voice> <out.wav> <text...>")
    };
    let norwegian = voice.starts_with("no_") || voice.starts_with("nb_");
    let mut tts = WyomingTts {
        address: address.clone(),
        english_voice: Some(voice.clone()),
        norwegian_voice: Some(voice.clone()),
    };
    let lang = if norwegian { Lang::Norwegian } else { Lang::English };
    let start = std::time::Instant::now();
    let mut pcm = Vec::new();
    tts.speak(&text.join(" "), lang, &mut |chunk| pcm.extend_from_slice(chunk))?;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: voice_proto::SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(out, spec)?;
    pcm.iter().try_for_each(|&s| writer.write_sample(s))?;
    writer.finalize()?;
    let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    println!("{out}: {:.2} s of audio in {:?}, peak {peak}", pcm.len() as f32 / 24_000.0, start.elapsed());
    Ok(())
}

//! Speed and agreement of Whisper with less than the full 30 s window encoded.
//! `cargo run --release -p voice-assistant --example whisper_window -- full,3/10,2/0 a.wav b.wav ...`
//! Each wav is 24 kHz mono. `full` is the 30 s window; `tail/min` encodes `tail` seconds of
//! silence after the utterance and at least `min` seconds. Transcripts that differ from the first
//! setting are marked `*`, and `!` when the rules parse them to a different intent.

use std::time::Instant;

use anyhow::{Context, Result};
use voice_assistant::intent;
use voice_assistant::lang::Lang;
use voice_assistant::whisper::{default_repo, Transcriber, WakePrompt, WhisperFiles, Window};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [settings, wavs @ ..] = args.as_slice() else {
        anyhow::bail!("usage: whisper_window full,3/10 a.wav b.wav ...")
    };
    let size = std::env::var("WHISPER_SIZE").unwrap_or_else(|_| "base".into());
    let files = WhisperFiles::fetch(&default_repo(false, &size))?;
    let audio: Vec<Vec<i16>> = wavs
        .iter()
        .map(|path| -> Result<Vec<i16>> {
            Ok(hound::WavReader::open(path)
                .with_context(|| path.clone())?
                .samples::<i16>()
                .collect::<Result<_, _>>()?)
        })
        .collect::<Result<_>>()?;
    // Transcript and the intent it parses to, per file, from the first setting.
    let mut reference: Vec<(String, String)> = Vec::new();
    for setting in settings.split(',') {
        let window = match setting.split_once('/') {
            Some((tail, min)) => Some(Window { tail: tail.parse()?, min: min.parse()? }),
            None => None,
        };
        let mut t = Transcriber::load(&files, None, voice_assistant::stt::device(false)?)?
            .with_wake_prompt(WakePrompt::Context, "Homie")
            .with_window(window);
        t.restrict(Lang::English);
        t.transcribe(&[0i16; 24_000])?;
        let (mut took, mut differ, mut intents, mut right, mut known) = (0.0, 0, 0, 0, 0);
        println!("{setting}");
        for (i, (path, pcm)) in wavs.iter().zip(&audio).enumerate() {
            let start = Instant::now();
            let text = t.transcribe(pcm)?.first().map(|c| c.text.clone()).unwrap_or_default();
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            took += ms;
            let parse = |text: &str| {
                let request = text.split_once("Homie,").map_or(text, |(_, rest)| rest);
                format!("{:?}", intent::parse(request))
            };
            let parsed = parse(&text);
            if let Ok(truth) = std::fs::read_to_string(std::path::Path::new(path).with_extension("txt")) {
                known += 1;
                right += usize::from(parse(truth.trim()) == parsed);
            }
            let (same, same_intent) = reference.get(i).map_or((true, true), |(t, p)| (*t == text, *p == parsed));
            differ += usize::from(!same);
            intents += usize::from(!same_intent);
            let mark = if !same_intent {
                '!'
            } else if !same {
                '*'
            } else {
                ' '
            };
            let name = path.rsplit('/').next().unwrap_or(path);
            println!("  {ms:>6.0} ms {:>4.1} s {mark} {name:<34} {text}", pcm.len() as f32 / 24_000.0);
            if reference.len() <= i {
                reference.push((text, parsed));
            }
        }
        let mean = took / wavs.len() as f64;
        println!("  mean {mean:.0} ms; differ from the first setting: {differ} transcripts (*), {intents} intents (!)");
        println!("  right intent, where a .txt next to the wav has the sentence: {right}/{known}\n");
    }
    Ok(())
}

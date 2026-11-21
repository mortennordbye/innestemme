//! Which pauses in a request the end of the turn could be called at sooner. Runs the energy
//! detector over each satellite recording (16 kHz mono, `dump-utterances` runs) as the engine does,
//! transcribes each draft with the engine's Parakeet and prints, per draft: whether the speaker went
//! on after the pause (`mid`) or the request ended there (`end`), whether the draft is acted on at
//! once (`complete_at_once`), and the draft's text and intent:
//! `cargo run --release -p voice-assistant --example end_turn -- Jarvis target/testset/runs/run-*.wav`.
//! The counts at the end are what matters: a rule that acts on a `mid` draft cuts the speaker off.
//! Only the first request of each run counts; after it the run holds the answer from the speaker.
//! PARAKEET and THREADS as for `wake_text`.

use anyhow::{bail, Result};
use voice_assistant::dialog::WakeWord;
use voice_assistant::intent::{self, Intent};
use voice_assistant::resample::to_24k;
use voice_assistant::vad::{Vad, DRAFT_FRAMES, END_FRAMES};
use voice_assistant::whisper::Transcribe;
use voice_proto::{FRAME_SAMPLES, SAMPLE_RATE};

mod common;

/// What the engine's draft handling sees in a transcript: the request after the name, if any. A
/// run starts at a wake word, so while it lasts words without the name are a request too.
fn request(wake: &WakeWord, text: &str) -> Option<String> {
    let rest = wake.strip(text).unwrap_or_else(|| text.to_owned());
    let rest = wake.drop_trailing(&rest);
    // "Hey Dorvis": the wake word itself, the name misheard.
    let words = voice_assistant::dialog::words(&rest);
    let wake_only = matches!(words.as_slice(), [greeting, _] if ["hey", "hi", "hei"].contains(&greeting.as_str()));
    (!words.is_empty() && !wake_only).then_some(rest)
}

/// The draft is acted on: what `act_on_draft` in the engine checks, apart from the conversation.
fn acted_on(request: &str) -> bool {
    let ends_on_and = request.split_whitespace().last().map(voice_assistant::dialog::normalize);
    !ends_on_and.is_some_and(|w| w == "and" || w == "og")
        && intent::split_commands(request).is_none()
        && intent::parse(request).meta().complete_at_once
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(name) = args.next() else { bail!("usage: end_turn <wake name> <wav>...") };
    let wake = WakeWord::new(&name);
    let mut parakeet = common::parakeet_from_env()?;
    let seconds = |samples: usize| samples as f32 / SAMPLE_RATE as f32;
    let (mut requests, mut ends, mut mids) = (0, 0, 0);
    let (mut ends_acted, mut mids_acted, mut ends_known, mut mids_known) = (0, 0, 0, 0);
    for path in args {
        let file = path.rsplit('/').next().unwrap_or(&path).to_owned();
        let pcm = to_24k(&voice_rt::wav::read_mono(&path, 16_000)?, 16_000);
        let mut vad = Vad::default();
        let mut drafts: Vec<(usize, Vec<i16>)> = Vec::new();
        let mut at = 0;
        for frame in pcm.as_chunks::<FRAME_SAMPLES>().0 {
            at += FRAME_SAMPLES;
            let ended = vad.push(frame);
            if let Some(draft) = vad.draft() {
                drafts.push((at, draft));
            }
            let Some(utterance) = ended else { continue };
            let text = parakeet.transcribe(&utterance)?.first().map(|t| t.text.clone()).unwrap_or_default();
            if std::env::var_os("VERBOSE").is_some() {
                eprintln!("{file}\t{:.2}s\tutterance\t{text}", seconds(at));
            }
            let Some(whole) = request(&wake, &text) else {
                drafts.clear();
                continue;
            };
            requests += 1;
            println!("{file}\t{:.2}s\trequest\t{whole}", seconds(at));
            let closing = (END_FRAMES - DRAFT_FRAMES) as usize * FRAME_SAMPLES;
            for (draft_at, draft) in drafts.drain(..) {
                let end = utterance.len() == draft.len() + closing;
                let text = parakeet.transcribe(&draft)?.first().map(|t| t.text.clone()).unwrap_or_default();
                let Some(asked) = request(&wake, &text) else {
                    println!("{file}\t{:.2}s\t{}\t-\t(no request yet)\t{text}", seconds(draft_at), kind(end));
                    count(end, &mut ends, &mut mids);
                    continue;
                };
                let parsed = intent::parse(&asked);
                let acted = acted_on(&asked);
                count(end, &mut ends, &mut mids);
                if acted {
                    count(end, &mut ends_acted, &mut mids_acted);
                }
                if parsed != Intent::Unknown {
                    count(end, &mut ends_known, &mut mids_known);
                }
                println!(
                    "{file}\t{:.2}s\t{}\t{}\t{asked}\t{parsed:?}",
                    seconds(draft_at),
                    kind(end),
                    if acted { "at once" } else { "waits" },
                );
            }
            break;
        }
    }
    println!(
        "{requests} requests; {ends} drafts at the real end ({ends_acted} acted on at once, {ends_known} parse to a \
         known intent); {mids} mid pauses ({mids_acted} acted on at once, {mids_known} parse to a known intent)"
    );
    Ok(())
}

fn kind(end: bool) -> &'static str {
    if end {
        "end"
    } else {
        "mid"
    }
}

fn count(end: bool, ends: &mut usize, mids: &mut usize) {
    if end {
        *ends += 1;
    } else {
        *mids += 1;
    }
}

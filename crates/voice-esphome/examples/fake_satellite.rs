//! A pretend voice satellite for testing the engine without hardware: serves the ESPHome API
//! (plaintext), streams a 16 kHz mono wav as its microphone in "wake word in Home Assistant" mode,
//! prints the events it receives, and saves the answer (fetched from the URL it is given, or
//! streamed with `--speaker`). Like the firmware, it starts playing the URL from the run's start
//! when told `tts_start_streaming`, and prints when the first audio arrived. `--timers` claims a
//! timer display; `--stay <s>` keeps the connection that long after the answer, to see timers end
//! and announcements arrive; `--then <wav>` is the reply when the engine keeps the conversation
//! open (a new run without the wake word, once the answer has played).
//!
//! `cargo run -p voice-esphome --example fake_satellite -- 127.0.0.1:16053 question-16k.wav answer.wav [--speaker] [--timers] [--stay 15] [--then reply-16k.wav]`

use std::io::{BufRead, BufReader, Read, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::net::TcpListener;
use voice_esphome::frame;
use voice_esphome::proto::{
    self, feature, fields, id, Announce, TimerUpdate, VoiceAssistantAudio, VoiceAssistantRequest, Writer,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [bind, wav, out, rest @ ..] = args.as_slice() else {
        bail!("usage: fake_satellite <bind host:port> <in-16k.wav> <answer.wav> [--speaker] [--timers] [--stay <s>] [--then <wav>]")
    };
    let speaker = rest.iter().any(|a| a == "--speaker");
    let timers = rest.iter().any(|a| a == "--timers");
    let stay: f32 = match rest.iter().position(|a| a == "--stay") {
        Some(i) => rest.get(i + 1).context("--stay needs seconds")?.parse()?,
        None => 0.0,
    };
    let read_wav = |path: &str| -> Result<Vec<i16>> {
        Ok(hound::WavReader::open(path)?.samples::<i16>().collect::<Result<_, _>>()?)
    };
    let mut mic = read_wav(wav)?;
    let mut then = match rest.iter().position(|a| a == "--then") {
        Some(i) => Some(read_wav(rest.get(i + 1).context("--then needs a wav")?)?),
        None => None,
    };

    let listener = TcpListener::bind(bind).await?;
    println!("fake satellite on {bind}, waiting for the engine");
    let (stream, peer) = listener.accept().await?;
    println!("engine connected from {peer}");
    let (mut r, mut w, _) = frame::open(stream, None).await?;

    loop {
        let (kind, _) = r.read().await?;
        match kind {
            id::HELLO_REQUEST => {
                let hello = Writer::default().uint(1, 1).uint(2, 12).str(3, "fake").str(4, "fake-satellite").finish();
                w.write(id::HELLO_RESPONSE, &hello).await?;
            }
            id::DEVICE_INFO_REQUEST => {
                let mut flags = feature::VOICE_ASSISTANT | feature::API_AUDIO | feature::ANNOUNCE;
                if speaker {
                    flags |= feature::SPEAKER;
                }
                if timers {
                    flags |= feature::TIMERS;
                }
                let info = Writer::default()
                    .str(2, "fake-satellite")
                    .str(6, "fake")
                    .str(13, "Fake Satellite")
                    .uint(17, flags as u64)
                    .finish();
                w.write(id::DEVICE_INFO_RESPONSE, &info).await?;
            }
            id::SUBSCRIBE_VOICE_ASSISTANT_REQUEST => break,
            _ => {}
        }
    }
    println!("subscribed; starting a continuous run");
    let start = VoiceAssistantRequest { start: true, flags: proto::REQUEST_USE_WAKE_WORD, ..Default::default() };
    w.write(id::VOICE_ASSISTANT_REQUEST, &start.encode()).await?;

    let t0 = Instant::now();
    let mut sent = 0usize;
    let mut streaming = true;
    let mut answer: Vec<i16> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(64));
    let mut housekeeping = tokio::time::interval(Duration::from_millis(50));
    let mut leave_at: Option<Instant> = None;
    // The URL from the run's start, the conversation staying open, and when the answer has played.
    let (mut run_url, mut keep_open, mut played_at) = (None::<String>, false, None::<Instant>);
    let (mut restart, mut playing_stream, mut quit_when_played) = (false, false, false);
    let (fetched_tx, mut fetched) = tokio::sync::mpsc::unbounded_channel();
    loop {
        if leave_at.is_some_and(|at| Instant::now() >= at) {
            break;
        }
        tokio::select! {
            _ = housekeeping.tick() => {
                // The firmware starts listening again once the answer has played.
                if restart && played_at.is_some_and(|at| Instant::now() >= at) {
                    let at = t0.elapsed().as_secs_f32();
                    println!("{at:6.2}s answer played; the conversation continues without the wake word");
                    (restart, played_at, keep_open) = (false, None, false);
                    mic = then.take().unwrap_or_default();
                    (sent, streaming) = (0, true);
                    // Real time from here: the ticks missed while waiting must not come in a burst.
                    tick.reset();
                    let start = VoiceAssistantRequest { start: true, flags: 0, ..Default::default() };
                    w.write(id::VOICE_ASSISTANT_REQUEST, &start.encode()).await?;
                }
            }
            Some((began, result)) = fetched.recv() => {
                let (audio, first): (Vec<i16>, Duration) = result?;
                let at = t0.elapsed().as_secs_f32();
                let seconds = audio.len() as f32 / 24_000.0;
                println!("{at:6.2}s streamed answer complete: {seconds:.2} s of audio, first audio {} ms after the go", first.as_millis());
                played_at = Some(began + first + Duration::from_secs_f32(seconds) + Duration::from_millis(300));
                answer.extend(audio);
                if quit_when_played {
                    break;
                }
            }
            _ = tick.tick(), if streaming => {
                // The microphone, at real time, then silence.
                let chunk: Vec<i16> = (0..1024).map(|i| mic.get(sent + i).copied().unwrap_or(0)).collect();
                sent += 1024;
                let data = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                w.write(id::VOICE_ASSISTANT_AUDIO, &VoiceAssistantAudio { data, end: false }.encode()).await?;
                if sent > mic.len() + 16_000 * 20 {
                    bail!("no answer within 20 s of the end of the question");
                }
            }
            message = r.read() => {
                let (kind, payload) = message?;
                let at = t0.elapsed().as_secs_f32();
                match kind {
                    id::VOICE_ASSISTANT_RESPONSE => println!("{at:6.2}s run accepted"),
                    id::VOICE_ASSISTANT_EVENT_RESPONSE => {
                        let f = fields(&payload)?;
                        let event = f.iter().find(|(n, _)| *n == 1).map_or(0, |(_, v)| v.uint());
                        let data: Vec<(String, String)> = f
                            .iter()
                            .filter(|(n, _)| *n == 2)
                            .map(|(_, v)| {
                                let inner = fields(v.bytes()).unwrap_or_default();
                                let get = |k| inner.iter().find(|(n, _)| *n == k).map(|(_, v)| v.string()).unwrap_or_default();
                                (get(1), get(2))
                            })
                            .collect();
                        println!("{at:6.2}s event {} {data:?}", name(event));
                        let url = data.iter().find(|(k, _)| k == "url").map(|(_, v)| v.clone());
                        match event {
                            1 => (run_url, playing_stream) = (url, false),
                            // Speech ended: a real device stops streaming here.
                            12 => streaming = false,
                            6 => keep_open = data.iter().any(|(k, v)| k == "continue_conversation" && v == "1"),
                            100 if data.iter().any(|(k, v)| k == "tts_start_streaming" && v == "1") => {
                                let url = run_url.take().context("tts_start_streaming without a URL at the run's start")?;
                                playing_stream = true;
                                let (tx, began) = (fetched_tx.clone(), Instant::now());
                                std::thread::spawn(move || {
                                    let _ = tx.send((began, fetch_stream(&url)));
                                });
                            }
                            8 if playing_stream => {
                                println!("{at:6.2}s already playing the streamed answer; TTS_END URL ignored");
                            }
                            8 => {
                                if let Some(url) = url {
                                    let audio = fetch_wav(&url)?;
                                    let seconds = audio.len() as f32 / 24_000.0;
                                    println!("{at:6.2}s fetched {url}: {seconds:.2} s of audio");
                                    played_at = Some(Instant::now() + Duration::from_secs_f32(seconds));
                                    answer.extend(audio);
                                }
                            }
                            2 if keep_open && then.is_some() => restart = true,
                            2 if stay > 0.0 && leave_at.is_none() => {
                                println!("{at:6.2}s staying connected for {stay} s");
                                leave_at = Some(Instant::now() + Duration::from_secs_f32(stay));
                            }
                            // The run ends before a streamed answer has finished arriving.
                            2 if leave_at.is_none() && playing_stream && played_at.is_none() => quit_when_played = true,
                            2 if leave_at.is_none() => break,
                            _ => {}
                        }
                    }
                    id::VOICE_ASSISTANT_AUDIO => {
                        let audio = VoiceAssistantAudio::decode(&payload)?;
                        let (samples, _) = audio.data.as_chunks::<2>();
                        answer.extend(samples.iter().map(|b| i16::from_le_bytes(*b)));
                    }
                    id::VOICE_ASSISTANT_TIMER_EVENT_RESPONSE => {
                        let t = TimerUpdate::decode(&payload)?;
                        let event = ["STARTED", "UPDATED", "CANCELLED", "FINISHED"].get(t.event as usize).unwrap_or(&"?");
                        println!(
                            "{at:6.2}s timer {event} id={} name={:?} total={}s left={}s active={}",
                            t.timer_id, t.name, t.total_seconds, t.seconds_left, t.is_active
                        );
                    }
                    id::VOICE_ASSISTANT_ANNOUNCE_REQUEST => {
                        let a = Announce::decode(&payload)?;
                        let audio = fetch_wav(&a.media_id)?;
                        println!("{at:6.2}s announce {:?}: {:.2} s of audio from {}", a.text, audio.len() as f32 / 24_000.0, a.media_id);
                        let finished = Writer::default().bool(1, true).finish();
                        w.write(id::VOICE_ASSISTANT_ANNOUNCE_FINISHED, &finished).await?;
                    }
                    id::PING_REQUEST => w.write(id::PING_RESPONSE, &[]).await?,
                    _ => {}
                }
            }
        }
    }
    let rate = if speaker { 16_000 } else { 24_000 };
    let spec =
        hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut writer = hound::WavWriter::create(out, spec)?;
    answer.iter().try_for_each(|&s| writer.write_sample(s))?;
    writer.finalize()?;
    println!("answer saved to {out} ({:.2} s at {rate} Hz)", answer.len() as f32 / rate as f32);
    Ok(())
}

fn name(event: u64) -> &'static str {
    match event {
        0 => "ERROR",
        1 => "RUN_START",
        2 => "RUN_END",
        3 => "STT_START",
        4 => "STT_END",
        5 => "INTENT_START",
        6 => "INTENT_END",
        7 => "TTS_START",
        8 => "TTS_END",
        9 => "WAKE_WORD_START",
        10 => "WAKE_WORD_END",
        11 => "STT_VAD_START",
        12 => "STT_VAD_END",
        98 => "TTS_STREAM_START",
        99 => "TTS_STREAM_END",
        100 => "INTENT_PROGRESS",
        _ => "?",
    }
}

/// Plays a URL as it arrives: its samples, and how long the first audio took.
fn fetch_stream(url: &str) -> Result<(Vec<i16>, Duration)> {
    let start = Instant::now();
    let rest = url.strip_prefix("http://").context("only http URLs")?;
    let (host, path) = rest.split_once('/').context("URL without a path")?;
    let mut stream = std::net::TcpStream::connect(host)?;
    write!(stream, "GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut chunked = false;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line.to_ascii_lowercase().starts_with("transfer-encoding: chunked") {
            chunked = true;
        }
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    let mut body = Vec::new();
    let mut first = None;
    if chunked {
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            let len = usize::from_str_radix(line.trim(), 16).context("bad chunk size")?;
            if len == 0 {
                break;
            }
            let mut chunk = vec![0u8; len + 2];
            reader.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk[..len]);
            if body.len() > 44 && first.is_none() {
                first = Some(start.elapsed());
            }
        }
    } else {
        reader.read_to_end(&mut body)?;
        first = Some(start.elapsed());
    }
    // Our own 44-byte header; its lengths are unknown while streaming.
    let (samples, _) = body.get(44..).unwrap_or_default().as_chunks::<2>();
    Ok((samples.iter().map(|b| i16::from_le_bytes(*b)).collect(), first.unwrap_or_default()))
}

/// A plain HTTP GET of a wav; its samples.
fn fetch_wav(url: &str) -> Result<Vec<i16>> {
    let rest = url.strip_prefix("http://").context("only http URLs")?;
    let (host, path) = rest.split_once('/').context("URL without a path")?;
    let mut stream = std::net::TcpStream::connect(host)?;
    write!(stream, "GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").context("no HTTP header end")? + 4;
    if !response.starts_with(b"HTTP/1.1 200") {
        bail!("{}", String::from_utf8_lossy(&response[..split]));
    }
    let reader = hound::WavReader::new(std::io::Cursor::new(response[split..].to_vec()))?;
    Ok(reader.into_samples::<i16>().collect::<Result<_, _>>()?)
}

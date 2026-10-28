//! Party mode from this computer's microphone, to try it without the satellite: music playing in the
//! room drives the Hue lights. Needs `HUE_BRIDGE`, `HUE_APP_KEY` and `HUE_CLIENT_KEY` (`.env`):
//! `set -a; . ./.env; set +a; cargo run --release -p voice-engine --example party -- normal 60`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use voice_assistant::resample::Stream;
use voice_engine::party::{HueConfig, Mode, Party};

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        Some("fast") => Mode::Fast,
        Some("relaxed") => Mode::Relaxed,
        Some("normal") | None => Mode::Normal,
        Some(other) => bail!("mode is normal, fast or relaxed, not {other}"),
    };
    let seconds: u64 = args.next().map_or(Ok(60), |s| s.parse()).context("seconds")?;
    let env = |name: &str| std::env::var(name).with_context(|| format!("{name} is not set"));
    let party = Party::new(Some(HueConfig {
        bridge: env("HUE_BRIDGE")?,
        app_key: env("HUE_APP_KEY")?,
        client_key: env("HUE_CLIENT_KEY")?,
    }));

    let mic = cpal::default_host().default_input_device().context("no microphone")?;
    let config: cpal::StreamConfig = mic.default_input_config()?.into();
    let (rate, channels) = (config.sample_rate, config.channels as usize);
    let (tx, rx) = mpsc::channel::<Vec<i16>>();
    let stream = mic.build_input_stream(
        config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let mono: Vec<i16> =
                data.chunks(channels).map(|f| (f[0] * 32767.0).clamp(-32768.0, 32767.0) as i16).collect();
            let _ = tx.send(mono);
        },
        |error| eprintln!("microphone: {error}"),
        None,
    )?;
    stream.play()?;

    party.start(mode)?;
    let mut down = Stream::new(rate, 16_000);
    let mut pcm = Vec::new();
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end && party.mode().is_some() {
        let Ok(block) = rx.recv_timeout(Duration::from_millis(100)) else { continue };
        pcm.clear();
        down.push(&block, &mut pcm);
        party.feed(&pcm);
    }
    party.stop();
    // The stream ends on its next frame and tells the bridge to stop.
    std::thread::sleep(Duration::from_secs(1));
    Ok(())
}

//! Runs one music request against the real Home Assistant, without audio:
//! `cargo run -p voice-assistant --example music -- "play my liked songs"`.
//! Reads HA_URL and HA_TOKEN from the environment (`set -a; . ./.env`), speaker from VOICE_SPEAKER.

use voice_assistant::ha::HomeAssistant;
use voice_assistant::intent::{self, Intent};
use voice_assistant::lang::Lang;
use voice_assistant::music::Player;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let text = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let Intent::Music(command) = intent::parse(&text) else { anyhow::bail!("not a music request: {text:?}") };
    let ha = HomeAssistant::new(&std::env::var("HA_URL")?, &std::env::var("HA_TOKEN")?);
    let mut player = Player::new(std::env::var("VOICE_SPEAKER").ok());
    // DRY_RUN=1: find what would play without starting it.
    if std::env::var_os("DRY_RUN").is_some() {
        player = player.dry_run();
    }
    println!("player: {}", player.check(&ha)?);
    let start = std::time::Instant::now();
    let answer = player.run(Some(&ha), &command, Lang::English);
    println!("{command:?} -> {answer:?} ({:?})", start.elapsed());
    Ok(())
}

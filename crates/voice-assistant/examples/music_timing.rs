//! How fast the speaker answers music commands through Music Assistant's player and through the
//! speaker's own player: `cargo run --release -p voice-assistant --example music_timing --
//! media_player.living_room_2 media_player.living_room`. Reads HA_URL and HA_TOKEN from the
//! environment (`set -a; . ./.env`). Music should be playing: it is paused and played, and the
//! volume nudged by one point and back. Each line is the service call's own time, then the time
//! until Home Assistant shows the new state.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use voice_assistant::ha::HomeAssistant;

/// Whether a state shows the service took effect.
type Done<'a> = dyn Fn(&Value) -> bool + 'a;

fn main() -> anyhow::Result<()> {
    let ha = HomeAssistant::new(&std::env::var("HA_URL")?, &std::env::var("HA_TOKEN")?);
    for entity in std::env::args().skip(1) {
        println!("{entity}");
        let volume = ha.state(&entity)?["attributes"]["volume_level"].as_f64().unwrap_or(0.3);
        let nudged = if volume < 0.99 { volume + 0.01 } else { volume - 0.01 };
        let steps: [(&str, Value, &Done<'_>); 4] = [
            ("media_pause", json!({}), &|s| s["state"] == "paused"),
            ("media_play", json!({}), &|s| s["state"] == "playing"),
            ("volume_set", json!({ "volume_level": nudged }), &|s| close(s, nudged)),
            ("volume_set", json!({ "volume_level": volume }), &|s| close(s, volume)),
        ];
        for (service, mut data, done) in steps {
            data["entity_id"] = entity.clone().into();
            let start = Instant::now();
            ha.service("media_player", service, data)?;
            let called = start.elapsed();
            let settled = settle(&ha, &entity, done)?;
            println!("  {service:<12} call {:>5} ms  settled {}", called.as_millis(), settled);
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    Ok(())
}

fn close(state: &Value, level: f64) -> bool {
    state["attributes"]["volume_level"].as_f64().is_some_and(|v| (v - level).abs() < 0.005)
}

/// Polls the state until `done`; how long it took from the end of the call.
fn settle(ha: &HomeAssistant, entity: &str, done: &Done<'_>) -> anyhow::Result<String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if done(&ha.state(entity)?) {
            return Ok(format!("{:>5} ms", start.elapsed().as_millis()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok("never (5 s)".into())
}

//! The lockdown and standing down, and the music after an answer.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tracing::{info, warn};
use voice_assistant::ha::{self, HomeAssistant};
use voice_assistant::intent::MusicCommand;
use voice_assistant::lang::Lang;
use voice_assistant::music::Player;
use voice_assistant::{house, tv};

use super::{unreachable, Worker};

/// The Hue alert the lockdown's lights pulse with lasts about 15 seconds: started again before it ends.
const ALARM_REPEAT: Duration = Duration::from_secs(14);
/// A lockdown nobody stands down stops blinking after this and stays red.
const ALARM_LONGEST: Duration = Duration::from_secs(30 * 60);

/// The lockdown's red lights blinking, on a thread of their own with their own Home Assistant
/// client, so answers are not held up.
pub(super) struct Alarm {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Alarm {
    fn start(ha: HomeAssistant, red: Vec<String>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::Builder::new().name("alarm".into()).spawn(move || {
            // The lights go red on the write queue; a pulse before that would flash the old colours.
            ha.flush();
            let start = Instant::now();
            let targets = house::alarm_targets(&ha, &red).unwrap_or_else(|error| {
                warn!(error = format!("{error:#}"), "no Hue rooms for the lockdown; each light alone");
                red
            });
            info!(?targets, "lockdown blink");
            while !stopped.load(Relaxed) && start.elapsed() < ALARM_LONGEST {
                if let Err(error) = house::alarm_pulse(&ha, &targets) {
                    warn!(error = format!("{error:#}"), "lockdown blink failed");
                }
                // In short sleeps, so standing down is not kept waiting.
                let pulse = Instant::now();
                while !stopped.load(Relaxed) && pulse.elapsed() < ALARM_REPEAT {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            info!(after = ?start.elapsed(), "lockdown blink stopped");
        })?;
        Ok(Self { stop, thread })
    }

    /// Waits for a pulse in flight, so nothing is started after this returns.
    fn stop(self) {
        self.stop.store(true, Relaxed);
        let _ = self.thread.join();
    }
}

impl Worker {
    /// Every light red, the volume up and the lockdown music on. Just for fun: nothing is locked.
    pub(super) fn lockdown(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        // Its red lights and the party's stream would fight over the same lights.
        if self.party_off_for_lockdown() {
            info!("party mode ended for the lockdown");
        }
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        match house::lockdown(ha) {
            Ok(red) => {
                info!(red = red.len(), "lockdown");
                let blinking = self.alarm.as_ref().is_some_and(|a| !a.thread.is_finished());
                if !blinking && !red.is_empty() {
                    match Alarm::start(ha.detached(), red) {
                        Ok(alarm) => self.alarm = Some(alarm),
                        Err(error) => warn!(%error, "could not start the lockdown's blink"),
                    }
                }
            }
            Err(error) => return unreachable(&error, no),
        }
        let mut level = f64::from(self.config.lockdown_volume) / 100.0;
        if let Ok(entity) = self.player.check(ha) {
            let volume = ha.state(&entity).ok().and_then(|s| s["attributes"]["volume_level"].as_f64());
            // Music already playing louder than the setting stays as loud: a lockdown never quietens it.
            level = level.max(volume.unwrap_or(0.0));
            // A second lockdown keeps the volume from before the first.
            self.lockdown_volume = self.lockdown_volume.or(volume);
        }
        self.lockdown_screen();
        // The lockdown's music replaces what was paused for the request.
        self.hushed = None;
        let music = MusicCommand::Song(self.config.lockdown_music.clone());
        self.after_answer = Some((music, Some(level)));
        self.music_under_answer = true;
        // When it began, so a TV that opens the screen later still counts from the start.
        let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        if !self.screen.showing("lockdown") {
            self.show("lockdown", serde_json::json!({ "since": since }), None);
        }
        if no {
            return "Nedstengning startet. Huset er forseglet.".into();
        }
        let threat = house::LOCKDOWN_THREATS[self.threat_turn % house::LOCKDOWN_THREATS.len()];
        self.threat_turn += 1;
        format!("Lockdown initiated. The house is sealed. {threat}")
    }

    /// Puts the lockdown's screen on a TV that is on and not showing it yet. On a thread of its own:
    /// the TV takes a while to answer, and the answer need not wait for it.
    fn lockdown_screen(&mut self) {
        let Some(url) = self.config.screen_url.clone() else { return };
        let Some(ha) = &self.home_assistant else { return };
        if self.lockdown_tv.is_some() || self.screen.watched() {
            return;
        }
        let (entity, source) = match self.tv.on_source(ha) {
            Ok(Some(on)) => on,
            Ok(None) => return,
            Err(error) => return warn!(error = format!("{error:#}"), "no tv for the lockdown"),
        };
        let (tv_ha, tv_entity) = (ha.detached(), entity.clone());
        let opened = std::thread::Builder::new().name("tv".into()).spawn(move || {
            if let Err(error) = tv::open_url(&tv_ha, &tv_entity, &url) {
                warn!(error = format!("{error:#}"), "could not open the lockdown on the tv");
            }
        });
        match opened {
            Ok(_) => self.lockdown_tv = Some((entity, source)),
            Err(error) => warn!(%error, "could not start the tv thread"),
        }
    }

    /// Starts or stops the music the last answer was about, and sets the volume, now that the
    /// answer has played. Volume first for new music, last when stopping, so neither is heard at
    /// the wrong level.
    pub(super) fn run_after_answer(&mut self) {
        self.after_answer_at = None;
        self.music_under_answer = false;
        let Some((music, volume)) = self.after_answer.take() else { return };
        let Some(ha) = &self.home_assistant else { return };
        let set_volume = |player: &mut Player| {
            let Some(level) = volume else { return };
            let set = player.check(ha).and_then(|entity| {
                ha.service(
                    "media_player",
                    "volume_set",
                    serde_json::json!({ "entity_id": entity, "volume_level": ha::volume_level(level * 100.0) }),
                )
            });
            if let Err(error) = set {
                warn!(error = format!("{error:#}"), "could not set the music's volume");
            }
        };
        let stopping = music == MusicCommand::Stop;
        if !stopping {
            set_volume(&mut self.player);
        }
        self.player.run(Some(ha), &music, Lang::English);
        if stopping {
            set_volume(&mut self.player);
        }
        info!(?music, ?volume, "music after the answer");
    }

    pub(super) fn stand_down(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        // Stopped first: a step after the lights are restored would turn them red again.
        if let Some(alarm) = self.alarm.take() {
            alarm.stop();
        }
        let restored = match house::stand_down(ha) {
            Ok(restored) => restored,
            Err(error) => return unreachable(&error, no),
        };
        // After the lockdown's snapshot, which holds the party's colours: the lights from before both.
        let party = std::mem::take(&mut self.party_under_lockdown)
            && house::party_restore(ha).unwrap_or_else(|error| {
                warn!(error = format!("{error:#}"), "could not put the lights back from before the party");
                false
            });
        let restored = restored || party;
        let volume = self.lockdown_volume.take();
        if !restored && volume.is_none() {
            return if no { "Det er ingen nedstengning å avslutte." } else { "There is no lockdown to end." }.into();
        }
        // The music was paused for the request: it stays paused under the answer, then stops.
        self.hushed = None;
        self.after_answer = Some((MusicCommand::Stop, volume));
        self.music_under_answer = false;
        info!(restored, ?volume, "lockdown over");
        // Back to what the TV showed, unless someone has since switched it to something else.
        if let Some((entity, Some(source))) = self.lockdown_tv.take().filter(|_| self.screen.watched()) {
            let tv_ha = ha.detached();
            let back = std::thread::Builder::new().name("tv".into()).spawn(move || {
                if let Err(error) = tv::select_source(&tv_ha, &entity, &source) {
                    warn!(error = format!("{error:#}"), "could not put the tv back");
                }
            });
            if let Err(error) = back {
                warn!(%error, "could not start the tv thread");
            }
        }
        if self.screen.showing("lockdown") {
            self.hide();
        }
        if no { "Avblåst. Alt er som før." } else { "Standing down. Everything is back as it was." }.into()
    }
}

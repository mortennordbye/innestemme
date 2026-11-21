//! The lockdown and standing down, and the music after an answer. The lockdown's Home Assistant
//! calls, its blinking and its answers are in [`voice_assistant::lockdown`]; what is here ends
//! party mode for it, puts the screen on the TV and plays the music after the answer, which all
//! need the engine's state.

use std::time::{Instant, SystemTime};

use tracing::{info, warn};
use voice_assistant::ha;
use voice_assistant::house;
use voice_assistant::intent::MusicCommand;
use voice_assistant::lang::Lang;
use voice_assistant::lockdown::{self, Alarm};
use voice_assistant::music::Player;
use voice_assistant::tv;

use super::{not_connected, unreachable, AfterAnswer, Worker};

impl Worker {
    /// Every light red, the volume up and the lockdown music on. Just for fun: nothing is locked.
    pub(super) fn lockdown(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        // Checked first: without a client nothing is locked down, and a party restore still
        // waiting should go ahead.
        if self.home_assistant.is_none() {
            return not_connected(no);
        }
        // Its red lights and the party's stream would fight over the same lights.
        let party = self.party_off_for_lockdown();
        // The lockdown's music and screen take over; standing down ends them.
        self.open_source_party = false;
        if party {
            info!("party mode ended for the lockdown");
        }
        let locked = lockdown::lockdown(self.home_assistant.as_ref().expect("checked above"));
        if party {
            self.party_kept_for_lockdown(locked.is_ok());
        }
        let ha = self.home_assistant.as_ref().expect("checked above");
        match locked {
            Ok(red) => {
                info!(red = red.len(), "lockdown");
                self.locked_down = true;
                // Undo would put a look from before the lockdown back over its red lights.
                self.light_undo_gone = Some(Instant::now());
                let blinking = self.alarm.as_ref().is_some_and(Alarm::blinking);
                if !blinking && !red.is_empty() {
                    match Alarm::start(ha.detached(), red) {
                        Ok(alarm) => self.alarm = Some(alarm),
                        Err(error) => warn!(%error, "could not start the lockdown's blink"),
                    }
                }
            }
            Err(error) => return unreachable(&error, no),
        }
        let (level, volume) = lockdown::music_level(ha, &mut self.player, self.config.lockdown_volume);
        // A second lockdown keeps the volume from before the first.
        self.lockdown_volume = self.lockdown_volume.or(volume);
        self.screen_on_tv();
        // The lockdown's music replaces what was paused for the request.
        self.conversation.hushed = None;
        let music = MusicCommand::Song(self.config.lockdown_music.clone());
        self.conversation.after_answer = Some(AfterAnswer::new(music, Some(level), true));
        // When it began, so a TV that opens the screen later still counts from the start.
        let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        if !self.screen.showing("lockdown") {
            self.show("lockdown", serde_json::json!({ "since": since }), None);
        }
        let answer = lockdown::started_answer(lang, self.threat_turn);
        // Norwegian has no threats to take turns.
        if !no {
            self.threat_turn += 1;
        }
        answer
    }

    /// Puts the screen on the TV, turned on first when it is off. On a thread of its own: the TV
    /// takes a while to answer, and the answer need not wait for it.
    pub(super) fn screen_on_tv(&mut self) {
        let Some(url) = self.config.screen_url.clone() else { return };
        let Some(ha) = &self.home_assistant else { return };
        if self.lockdown_tv.is_some() || self.screen.watched() {
            return;
        }
        let (entity, before) = match self.tv.before(ha) {
            Ok(Some(tv)) => tv,
            Ok(None) => return,
            Err(error) => return warn!(error = format!("{error:#}"), "no tv for the screen"),
        };
        let (tv_ha, tv_entity, off) = (ha.detached(), entity.clone(), before == tv::Before::Off);
        let opened = std::thread::Builder::new().name("tv".into()).spawn(move || {
            let open = || {
                if off {
                    tv_ha.service("media_player", "turn_on", serde_json::json!({ "entity_id": tv_entity }))?;
                    tv::wait_until_on(&tv_ha, &tv_entity)?;
                }
                tv::open_url(&tv_ha, &tv_entity, &url)
            };
            if let Err(error) = open() {
                warn!(error = format!("{error:#}"), "could not open the screen on the tv");
            }
        });
        match opened {
            Ok(_) => self.lockdown_tv = Some((entity, before)),
            Err(error) => warn!(%error, "could not start the tv thread"),
        }
    }

    /// Starts or stops the music the last answer was about, and sets the volume, now that the
    /// answer has played. Volume first for new music, last when stopping, so neither is heard at
    /// the wrong level.
    pub(super) fn run_after_answer(&mut self) {
        let Some(AfterAnswer { music, volume, .. }) = self.conversation.after_answer.take() else { return };
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

    /// Back to what the TV showed before [`Self::screen_on_tv`], unless someone has since switched
    /// it to something else.
    pub(super) fn tv_back(&mut self) {
        let Some(ha) = &self.home_assistant else { return };
        let Some((entity, before)) = self.lockdown_tv.take().filter(|_| self.screen.watched()) else {
            return;
        };
        let tv_ha = ha.detached();
        let back = std::thread::Builder::new().name("tv".into()).spawn(move || {
            let back = match before {
                tv::Before::Source(source) => tv::select_source(&tv_ha, &entity, &source),
                tv::Before::Off => {
                    tv_ha.service("media_player", "turn_off", serde_json::json!({ "entity_id": entity }))
                }
                tv::Before::Unknown => Ok(()),
            };
            if let Err(error) = back {
                warn!(error = format!("{error:#}"), "could not put the tv back");
            }
        });
        if let Err(error) = back {
            warn!(%error, "could not start the tv thread");
        }
    }

    pub(super) fn stand_down(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return not_connected(no);
        };
        // Only with a lockdown to end: "stand down" without one leaves a party and its restore be.
        // Home Assistant's scene decides; what this engine knows only when it can't be asked.
        let locked_down = lockdown::lockdown_on(ha, self.locked_down);
        if locked_down {
            self.party_off_for_stand_down();
        } else if self.open_source_party && self.party_off() {
            return if no { "Ok, festen er over." } else { "Okay, the party's over." }.into();
        }
        let ha = self.home_assistant.as_ref().expect("checked above");
        // Stopped first: a step after the lights are restored would turn them red again.
        if let Some(alarm) = self.alarm.take() {
            alarm.stop();
        }
        self.locked_down = false;
        if locked_down {
            self.light_undo_gone = Some(Instant::now());
        }
        let restored = match lockdown::stand_down(ha) {
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
            return lockdown::stood_down_answer(false, lang);
        }
        // The music was paused for the request: it stays paused under the answer, then stops.
        self.conversation.hushed = None;
        self.conversation.after_answer = Some(AfterAnswer::new(MusicCommand::Stop, volume, false));
        info!(restored, ?volume, "lockdown over");
        self.tv_back();
        if self.screen.showing("lockdown") {
            self.hide();
        }
        lockdown::stood_down_answer(true, lang)
    }
}

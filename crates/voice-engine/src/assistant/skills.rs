//! Skills answered by the worker itself: weather, people, briefing, distance, party; and scenes and
//! scripts, whose Home Assistant calls and answers are in [`voice_assistant::scenes`].

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use voice_assistant::ha::{self, HomeAssistant};
use voice_assistant::house;
use voice_assistant::intent::{Day, Focus, MusicCommand};
use voice_assistant::lang::Lang;
use voice_assistant::lockdown;
use voice_assistant::party::{Pace, PartyCommand};
use voice_assistant::scenes::{self, ByName};
use voice_assistant::shopping::ListCommand;
use voice_assistant::skill::Done;

use crate::party;

use super::{not_connected, time_of_day, unreachable, AfterAnswer, Worker};

/// How long the Hue bridge takes to let go of the lights once party mode's stream stops; calls
/// to them before then are lost.
const PARTY_RELEASE: Duration = Duration::from_secs(1);

/// What starting party mode keeps of the lights for its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotPlan {
    /// A lockdown is on: its snapshot (and a party's from before it) is the look to go back to,
    /// not the red lights now.
    Skip,
    /// Party mode is already on, or ended a moment ago with its restore called off: the snapshot
    /// from its start still holds the lights from before.
    KeepExisting,
    /// A new snapshot, replacing one left over from a restore whose delete failed.
    Fresh,
}

fn snapshot_plan(party_on: bool, locked_down: bool) -> SnapshotPlan {
    match (locked_down, party_on) {
        (true, _) => SnapshotPlan::Skip,
        (false, true) => SnapshotPlan::KeepExisting,
        (false, false) => SnapshotPlan::Fresh,
    }
}

/// Where the lights go once party mode's stream has let go of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterParty {
    /// Back to the snapshot from before the party.
    PartyScene,
    /// A party during a lockdown: back to red, the party's snapshot kept for standing down.
    LockdownRed,
}

fn after_party(locked_down: bool) -> AfterParty {
    if locked_down {
        AfterParty::LockdownRed
    } else {
        AfterParty::PartyScene
    }
}

/// Party mode's lights put back on a thread of its own after a wait; until then it can be called off.
pub(super) struct PendingRestore {
    cancelled: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
    /// When the party stopped.
    stopped: Instant,
}

impl PendingRestore {
    fn spawn(wait: Duration, restore: impl FnOnce() + Send + 'static) -> std::io::Result<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let called_off = cancelled.clone();
        let thread = std::thread::Builder::new().name("party-restore".into()).spawn(move || {
            // In short sleeps, so calling it off is not kept waiting.
            let start = Instant::now();
            while start.elapsed() < wait {
                if called_off.load(Relaxed) {
                    return;
                }
                std::thread::sleep(wait.saturating_sub(start.elapsed()).min(Duration::from_millis(20)));
            }
            // Taken in one step, so a restore and calling it off never both happen.
            if !called_off.swap(true, Relaxed) {
                restore();
            }
        })?;
        Ok(Self { cancelled, thread, stopped: Instant::now() })
    }

    /// Calls the restore off unless it has begun, and waits for one in flight. Whether it was
    /// called off in time.
    fn cancel(self) -> bool {
        let in_time = !self.cancelled.swap(true, Relaxed);
        let _ = self.thread.join();
        in_time
    }
}

impl Worker {
    /// "Party mode": the Hue lights follow the music, after a snapshot to put them back from.
    pub(super) fn party(&mut self, command: PartyCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        if command == PartyCommand::Status {
            return match (self.config.party.mode().is_some(), no) {
                (true, false) => "Party mode is on.",
                (false, false) => "Party mode is off.",
                (true, true) => "Festmodus er på.",
                (false, true) => "Festmodus er av.",
            }
            .into();
        }
        let PartyCommand::On(pace) = command else {
            return match self.party_off() {
                true if no => "Ok, festen er over.",
                true => "Okay, the party's over.",
                false if no => "Det er ingen fest på gang.",
                false => "There's no party on.",
            }
            .into();
        };
        if !self.config.party.configured() {
            return if no {
                "Festmodus trenger en Hue-bro. Sett hue-bridge i innstillingene."
            } else {
                "Party mode needs a Hue bridge. Set hue-bridge in the settings."
            }
            .into();
        }
        // A party that stopped by itself left this set; a plain party mode never stops the music.
        if self.config.party.mode().is_none() {
            self.open_source_party = false;
        }
        // A restore still waiting would land on the party's first frames. Called off in time, the
        // last party's snapshot is still the look from before it.
        let restore_called_off = self.party_restore.take().is_some_and(PendingRestore::cancel);
        if let Some(ha) = &self.home_assistant {
            let locked_down = lockdown::lockdown_on(ha, self.locked_down);
            self.locked_down = locked_down;
            // Left set by a lockdown that was stood down elsewhere, or one Home Assistant forgot.
            self.party_under_lockdown &= locked_down;
            let party_on = self.config.party.mode().is_some() || restore_called_off;
            let snapshot = match snapshot_plan(party_on, locked_down) {
                SnapshotPlan::Skip => Ok(()),
                SnapshotPlan::KeepExisting => house::party_snapshot(ha, true),
                SnapshotPlan::Fresh => house::party_snapshot(ha, false),
            };
            if let Err(error) = snapshot {
                warn!(error = format!("{error:#}"), "party mode: no snapshot of the lights");
            }
        }
        let mode = match pace {
            Pace::Relaxed => party::Mode::Relaxed,
            Pace::Normal => party::Mode::Normal,
            Pace::Fast => party::Mode::Fast,
        };
        if let Err(error) = self.config.party.start(mode) {
            warn!(error = format!("{error:#}"), "party mode did not start");
            // The last party's lights were never put back: no new party will do it.
            if restore_called_off {
                self.spawn_party_restore(PARTY_RELEASE);
            }
            return if no {
                "Beklager, jeg fikk ikke startet festmodus."
            } else {
                "Sorry, I couldn't start party mode."
            }
            .into();
        }
        // The stream changes the lights: undo has no look from before it to go back to.
        self.forget_light_undo();
        // Room talk over the music in a follow-up window would be taken as a request ("play").
        self.conversation.follow_up_closed = true;
        match (pace, no) {
            (Pace::Relaxed, false) => "Okay, relaxed party mode is on.",
            (Pace::Normal, false) => "Okay, party mode is on.",
            (Pace::Fast, false) => "Okay, fast party mode is on.",
            (Pace::Relaxed, true) => "Ok, rolig festmodus er på.",
            (Pace::Normal, true) => "Ok, festmodus er på.",
            (Pace::Fast, true) => "Ok, rask festmodus er på.",
        }
        .into()
    }

    /// Ends party mode and puts the lights back once the stream has let go of them. Whether it was on.
    pub(super) fn party_off(&mut self) -> bool {
        if self.config.party.mode().is_none() {
            return false;
        }
        self.config.party.stop();
        self.forget_light_undo();
        self.spawn_party_restore(PARTY_RELEASE);
        self.conversation.follow_up_closed = true;
        if std::mem::take(&mut self.open_source_party) {
            // The music was paused for the request: it stays paused under the answer, then stops.
            self.conversation.hushed = None;
            self.conversation.after_answer = Some(AfterAnswer::new(MusicCommand::Stop, None, false));
            // The lockdown's TV stays with it until standing down.
            if !self.locked_down {
                self.tv_back();
            }
        }
        self.hide_open_source();
        true
    }

    /// "What do you think about open source?": party mode, its song after the answer, and its
    /// screen until the party ends.
    pub(super) fn open_source(&mut self, lang: Lang) -> String {
        let started = self.party(PartyCommand::On(Pace::Normal), lang);
        if self.config.party.mode().is_none() {
            return started;
        }
        // Under the answer, like the lockdown's music: a start just as the answer's announcement ends
        // on the same speaker can hang in Music Assistant.
        let song = MusicCommand::Song(self.config.open_source_music.clone());
        self.conversation.after_answer = Some(AfterAnswer::new(song, None, true));
        self.open_source_party = true;
        self.screen_on_tv();
        self.show("open_source", serde_json::json!({}), None);
        if lang == Lang::Norwegian { "Jeg elsker åpen kildekode, for faen." } else { house::OPEN_SOURCE_ANSWER }.into()
    }

    fn hide_open_source(&mut self) {
        if self.screen.showing("open_source") {
            self.hide();
        }
    }

    /// Puts party mode's lights back after `wait`, on a thread of its own: the bridge lets go of
    /// the lights after the stream's last frame.
    pub(super) fn spawn_party_restore(&mut self, wait: Duration) {
        let Some((url, token)) = self.config.home_assistant.clone() else { return };
        let locked_here = self.locked_down;
        let restore = PendingRestore::spawn(wait, move || {
            let ha = HomeAssistant::new(&url, &token);
            let put_back = match after_party(lockdown::lockdown_on(&ha, locked_here)) {
                // Red again; the party's snapshot stays for standing down.
                AfterParty::LockdownRed => lockdown::lockdown(&ha).map(|_| true),
                AfterParty::PartyScene => house::party_restore(&ha),
            };
            if let Err(error) = put_back {
                warn!(error = format!("{error:#}"), "party mode: could not put the lights back");
            }
        });
        match restore {
            Ok(pending) => self.party_restore = Some(pending),
            Err(error) => warn!(%error, "party mode: could not put the lights back"),
        }
    }

    /// Ends party mode for a lockdown without putting its lights back: put back now, they would
    /// land on the lockdown's red lights, and the lockdown's own snapshot would hold the party's
    /// colours. Waits until the bridge has let go of the lights. A party ended a moment ago whose
    /// lights are not back yet counts too. Whether there was one; the lockdown takes its snapshot
    /// over with [`Self::party_kept_for_lockdown`], or puts it back if it fails.
    pub(super) fn party_off_for_lockdown(&mut self) -> bool {
        if let Some(pending) = self.party_restore.take() {
            let stopped = pending.stopped;
            if pending.cancel() {
                std::thread::sleep(PARTY_RELEASE.saturating_sub(stopped.elapsed()));
                return true;
            }
        }
        if self.config.party.mode().is_none() {
            return false;
        }
        self.config.party.stop();
        std::thread::sleep(PARTY_RELEASE);
        true
    }

    /// After [`Self::party_off_for_lockdown`]: the lockdown is on and the party's snapshot (the
    /// lights from before both) is kept for standing down; or it failed, and the lights from before
    /// the party go back now, the bridge having let go of them already.
    pub(super) fn party_kept_for_lockdown(&mut self, locked_down: bool) {
        if locked_down {
            self.party_under_lockdown = true;
        } else {
            self.spawn_party_restore(Duration::ZERO);
        }
    }

    /// Before standing down: a party started during the lockdown lets go of the lights first, or
    /// its stream would cover the restored look. A restore still waiting is dropped, since standing
    /// down puts the lights back itself.
    pub(super) fn party_off_for_stand_down(&mut self) {
        if let Some(pending) = self.party_restore.take() {
            pending.cancel();
        }
        if self.config.party.mode().is_some() {
            self.config.party.stop();
            info!("party mode ended for standing down");
            self.open_source_party = false;
            self.hide_open_source();
            std::thread::sleep(PARTY_RELEASE);
        }
    }

    /// A named place, else the home: the address when it is set, else the `home` place name.
    pub(super) fn weather(&mut self, place: Option<String>, day: Day, focus: Focus, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let named = place.or_else(|| self.home_location.is_none().then(|| self.config.home.clone()).flatten());
        let location = match named {
            None => match &self.home_location {
                Some(home) => Ok(Some(home.clone())),
                None if no => return "Hvilket sted? Si for eksempel været i Oslo.".into(),
                None => return "Which place? Say, for example, the weather in London.".into(),
            },
            Some(name) => self.weather.find(&name, self.home_location.as_ref()),
        };
        let report = location.and_then(|location| match location {
            Some(location) => self.weather.report(&location, day, focus, lang).map(Some),
            None => Ok(None),
        });
        match report {
            Ok(Some(report)) => report,
            Ok(None) if no => "Beklager, jeg fant ikke det stedet.".into(),
            Ok(None) => "Sorry, I couldn't find that place.".into(),
            Err(error) => {
                warn!(error = format!("{error:#}"), "weather lookup failed");
                if no {
                    "Beklager, jeg får ikke kontakt med værtjenesten.".into()
                } else {
                    "Sorry, I could not reach the weather service.".into()
                }
            }
        }
    }

    /// "Where is Ingrid?", "is Ingrid at work?", "when will Ingrid be home?"
    pub(super) fn where_is(&mut self, name: &str, zone: Option<&str>, arriving: bool, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return not_connected(no);
        };
        match ha.people() {
            Ok(people) => {
                voice_assistant::people::where_answer(&people, name, zone, arriving, self.home_location.as_ref(), lang)
            }
            Err(error) => unreachable(&error, no),
        }
    }

    /// "Who's home?": everyone and where they are; "is Ingrid home?": that person.
    pub(super) fn whos_home(&mut self, name: Option<&str>, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return not_connected(no);
        };
        let people = match ha.people() {
            Ok(people) => people,
            Err(error) => return unreachable(&error, no),
        };
        let first_name = |p: &ha::Person| p.name.split_whitespace().next().unwrap_or(&p.name).to_owned();
        let place = |p: &ha::Person| match (no, p.state.as_str()) {
            (false, "home") => "home".to_owned(),
            (true, "home") => "hjemme".to_owned(),
            (false, "not_home") => "out".to_owned(),
            (true, "not_home") => "ute".to_owned(),
            (false, "unknown" | "unavailable") => "somewhere I can't tell".to_owned(),
            (true, "unknown" | "unavailable") => "et sted jeg ikke vet".to_owned(),
            (false, zone) => format!("at {zone}"),
            (true, zone) => format!("på {zone}"),
        };
        if let Some(name) = name {
            let names = voice_assistant::names::Names::new(people.iter().map(first_name));
            let Some(person) = names.best(name).and_then(|found| people.iter().find(|p| first_name(p) == found.name))
            else {
                return if no { format!("Jeg vet ikke hvor {name} er.") } else { format!("I don't track {name}.") };
            };
            let who = first_name(person);
            return match (no, person.state == "home") {
                (false, true) => format!("Yes, {who} is home."),
                (true, true) => format!("Ja, {who} er hjemme."),
                (false, false) => format!("No, {who} is {}.", place(person)),
                (true, false) => format!("Nei, {who} er {}.", place(person)),
            };
        }
        let home: Vec<String> = people.iter().filter(|p| p.state == "home").map(first_name).collect();
        let join = |names: &[String]| voice_assistant::say::join(names, lang);
        let is = if no { "er" } else { "is" };
        let mut clauses: Vec<String> = Vec::new();
        match (no, home.len()) {
            (_, 0) => {}
            (false, n) => clauses.push(format!("{} {} home", join(&home), if n == 1 { "is" } else { "are" })),
            (true, _) => clauses.push(format!("{} er hjemme", join(&home))),
        }
        clauses.extend(
            people.iter().filter(|p| p.state != "home").map(|p| format!("{} {is} {}", first_name(p), place(p))),
        );
        match (no, home.is_empty()) {
            (false, true) if clauses.is_empty() => "Home Assistant tracks nobody.".into(),
            (true, true) if clauses.is_empty() => "Home Assistant følger ingen.".into(),
            (false, true) => format!("Nobody is home; {}.", clauses.join(", ")),
            (true, true) => format!("Ingen er hjemme; {}.", clauses.join(", ")),
            _ => format!("{}.", clauses.join("; ")),
        }
    }

    /// "Good morning": greeting and time, the weather at home, the shopping list, power prices.
    pub(super) fn briefing(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let now = jiff::Zoned::now();
        let greeting = match (no, now.hour()) {
            (false, 4..=11) => "Good morning",
            (false, 12..=17) => "Good afternoon",
            (false, _) => "Good evening",
            (true, 4..=9) => "God morgen",
            (true, 10..=17) => "Hei",
            (true, _) => "God kveld",
        };
        let mut parts = vec![time_of_day(lang).replacen("It's", &format!("{greeting}! It's"), 1)];
        if no {
            parts[0] = format!("{greeting}! {}", time_of_day(lang));
        }
        if self.home_location.is_some() || self.config.home.is_some() {
            parts.push(self.weather(None, Day::Today, Focus::General, lang));
        }
        if let Some(ha) = &self.home_assistant {
            if let Ok((answer, _)) = self.shopping.run(ha, &ListCommand::Read, lang) {
                // Only a list with something on it is news.
                if !answer.contains("empty") && !answer.contains("tom") {
                    parts.push(answer);
                }
            }
        }
        match self.news.briefing(lang) {
            Ok(headlines) => {
                self.live.extend(headlines.live);
                parts.push(headlines.text);
            }
            Err(error) => warn!(error = format!("{error:#}"), "no news for the briefing"),
        }
        parts.join(" ")
    }

    pub(super) fn distance(&mut self, query: &voice_assistant::distance::Query, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let from = match &query.from {
            Some(name) => self.weather.find(name, self.home_location.as_ref()),
            None => Ok(self.home_location.clone()),
        };
        let places = from.and_then(|from| Ok((from, self.weather.find(&query.to, self.home_location.as_ref())?)));
        match places {
            Ok((Some(from), Some(to))) => voice_assistant::distance::answer(&from, &to, lang),
            Ok((None, _)) if query.from.is_none() && no => "Hvor fra? Si for eksempel fra Oslo til Bergen.".into(),
            Ok((None, _)) if query.from.is_none() => "From where? Say, for example, from Oslo to Bergen.".into(),
            Ok(_) if no => "Beklager, jeg fant ikke det stedet.".into(),
            Ok(_) => "Sorry, I couldn't find that place.".into(),
            Err(error) => {
                warn!(error = format!("{error:#}"), "place lookup failed");
                if no {
                    "Beklager, jeg får ikke slått opp stedet nå."
                } else {
                    "Sorry, I couldn't look up the place right now."
                }
                .into()
            }
        }
    }

    pub(super) fn scene(&mut self, request: &str, lang: Lang) -> String {
        let room = self.room.clone();
        self.home_skill(lang, |ha| scenes::scene(ha, request, room.as_deref(), lang))
    }

    pub(super) fn script(&mut self, request: &str, lang: Lang) -> String {
        self.home_skill(lang, |ha| scenes::script(ha, request, lang).map(Done::say))
    }

    /// A script or scene whose whole name is the request; `None` when there is none.
    pub(super) fn by_exact_name(&mut self, request: &str, lang: Lang) -> Option<String> {
        Some(match scenes::by_exact_name(self.home_assistant.as_mut()?, request)? {
            ByName::Script => self.script(request, lang),
            ByName::Scene(name) => self.scene(&name, lang),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn a_party_during_a_lockdown_keeps_no_snapshot_of_the_red_lights() {
        assert_eq!(snapshot_plan(false, true), SnapshotPlan::Skip);
        assert_eq!(snapshot_plan(true, true), SnapshotPlan::Skip);
        // Party on, or a party stopped a moment ago whose restore was called off by this start.
        assert_eq!(snapshot_plan(true, false), SnapshotPlan::KeepExisting);
        // No lockdown on: a snapshot left from one never stood down is replaced.
        assert_eq!(snapshot_plan(false, false), SnapshotPlan::Fresh);
        assert_eq!(after_party(true), AfterParty::LockdownRed);
        assert_eq!(after_party(false), AfterParty::PartyScene);
    }

    #[test]
    fn a_restore_called_off_in_time_never_runs() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let pending = PendingRestore::spawn(Duration::from_millis(300), move || {
            counted.fetch_add(1, Relaxed);
        })
        .unwrap();
        let start = Instant::now();
        assert!(pending.cancel());
        // Not kept waiting for the whole wait.
        assert!(start.elapsed() < Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(350));
        assert_eq!(runs.load(Relaxed), 0);
    }

    #[test]
    fn a_restore_that_ran_is_not_called_off() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let pending = PendingRestore::spawn(Duration::ZERO, move || {
            counted.fetch_add(1, Relaxed);
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!pending.cancel());
        assert_eq!(runs.load(Relaxed), 1);
    }
}

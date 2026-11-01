//! Timers, reminders and wake-up alarms: durations and times of day from speech (English and
//! Norwegian, digits or words), the running timers, and what the assistant says about them.
//!
//! A reminder is a timer with a message: when it ends, the message is spoken instead of a ring. A
//! wake-up alarm is a timer set for a time of day that rings like one; "snooze" sets it again. Kept
//! in a file, they all survive a restart.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use jiff::{SignedDuration, Timestamp, Zoned};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::lang::Lang;

/// How long "snooze" puts the alarm off, in seconds.
pub const SNOOZE: u32 = 9 * 60;
/// An alarm counts as ringing this long after it went off: the Voice PE rings for 15 minutes at most.
const RINGS_FOR: Duration = Duration::from_secs(15 * 60);
/// Ended while the engine was down: a timer this late is still told, an alarm this late still rings.
/// Later ones are dropped; a reminder is always told.
const LATE_TIMER: Duration = Duration::from_secs(10 * 60);
const LATE_ALARM: Duration = Duration::from_secs(30 * 60);
/// After a restart, late ones wait this long for a satellite to connect and hear them.
const LATE_WAIT: Duration = Duration::from_secs(60);
/// Before this hour "tomorrow" still means the coming morning: said at 00:30, "wake me up tomorrow
/// at 7" is in six and a half hours, not thirty.
const NIGHT_ENDS: i8 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerCommand {
    /// `seconds` is `None` when the request names no duration ("set a timer"): ask for it.
    Start {
        seconds: Option<u32>,
        name: Option<String>,
    },
    /// "Remind me in 10 minutes to check the oven", "remind me at 5 to call mum"; either part can
    /// be missing.
    Remind {
        when: Option<When>,
        message: Option<String>,
    },
    /// A wake-up alarm: "wake me up at 7", "set an alarm for 6:30". `None`: ask for the time.
    Alarm {
        when: Option<When>,
    },
    /// The alarm that is set, to a new time ("move my alarm to 7:30") or later or earlier by a while
    /// ("push my alarm back 10 minutes"). While one rings, later is a snooze that long.
    MoveAlarm(Move),
    /// The alarm that just went off, again in `SNOOZE`.
    Snooze,
    /// "Turn off the alarm": the one ringing, else the alarm that is set.
    StopAlarm,
    Cancel(Which),
    Pause(Which),
    Resume(Which),
    Add {
        seconds: u32,
        which: Which,
    },
    /// How much time is left; also "what timers do I have", and "when is my alarm".
    Remaining(Which),
}

/// Where "move my alarm" puts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    To(When),
    /// Later (or, negative, earlier) by this many seconds.
    By(i64),
}

/// When a reminder or an alarm goes off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// In this many seconds.
    In(u32),
    At(Clock),
}

/// A time of day as said: "at 5", "at 5 pm", "at 17:30", "tomorrow at 8", "klokka 7".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    /// 0 to 23 as said: 1 to 12 without `pm` can be either half of the day.
    pub hour: u8,
    pub minute: u8,
    /// `Some(true)` for "pm" or "in the evening", `Some(false)` for "am"; `None` when not said.
    pub pm: Option<bool>,
    pub tomorrow: bool,
}

impl Clock {
    /// When it next comes after `now`. An hour without am or pm is whichever comes first, within 12
    /// hours; for an alarm it is the morning, and "tomorrow at 3" is the afternoon.
    pub fn next(&self, now: &Zoned, alarm: bool) -> Option<Zoned> {
        let h = self.hour;
        let hours = match self.pm {
            _ if h == 0 || h > 12 => vec![h],
            Some(true) => vec![h % 12 + 12],
            Some(false) => vec![h % 12],
            None if alarm => vec![h],
            None if self.tomorrow => vec![if h <= 6 { h + 12 } else { h }],
            None => vec![h % 12, h % 12 + 12],
        };
        let today = now.date();
        let tomorrow = today.tomorrow().ok()?;
        let days = if self.tomorrow && now.hour() >= NIGHT_ENDS { vec![tomorrow] } else { vec![today, tomorrow] };
        hours
            .iter()
            .filter_map(|&hour| {
                days.iter().find_map(|day| {
                    let at = day.at(hour as i8, self.minute as i8, 0, 0).to_zoned(now.time_zone().clone()).ok()?;
                    (at.timestamp() > now.timestamp()).then_some(at)
                })
            })
            .min_by_key(|at| at.timestamp())
    }
}

/// Which timers a request is about. Nothing set means "the timer", whichever there is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Which {
    /// "The pasta timer".
    pub name: Option<String>,
    /// "The 10 minute timer": the timer's full length.
    pub seconds: Option<u32>,
    /// "All timers", "alle timerne".
    pub all: bool,
    /// "The reminder", "påminnelsen".
    pub reminder: bool,
    /// "My alarm": wake-up alarms, which timer requests leave alone.
    pub alarm: bool,
}

const TIMER_WORDS: &[&str] = &["timer", "timers", "timeren", "timerne", "countdown", "nedtelling", "nedtellingen"];
const REMINDER_WORDS: &[&str] = &["reminder", "reminders", "påminnelse", "påminnelsen", "påminnelser", "påminnelsene"];
/// A wake-up alarm. Norwegian "alarmen" is the house's alarm system (`house.rs`).
const ALARM_WORDS: &[&str] =
    &["alarm", "alarms", "vekkerklokke", "vekkerklokka", "vekkerklokken", "vekking", "vekkingen", "vekkerklokkene"];
const SNOOZE_WORDS: &[&str] = &["snooze", "slumre", "slumring"];
/// The house's alarm system, which `house.rs` arms and reports on.
const HOUSE_ALARM: &[&str] = &[
    "arm",
    "armed",
    "arming",
    "disarm",
    "disarmed",
    "security",
    "burglar",
    "alarmen",
    "innbruddsalarm",
    "innbruddsalarmen",
    "boligalarmen",
];
/// Words before a time of day: "at 5", "klokka 7"; an alarm also "for 6:30", "to 7", "på 7".
const AT_WORDS: &[&str] = &["at", "klokka", "klokken", "kl"];
const ALARM_AT_WORDS: &[&str] = &["at", "klokka", "klokken", "kl", "for", "to", "til", "på", "until"];
const CANCEL_WORDS: &[&str] =
    &["cancel", "stop", "delete", "remove", "clear", "off", "avbryt", "stopp", "slett", "fjern"];
const PAUSE_WORDS: &[&str] = &["pause", "hold", "freeze"];
const RESUME_WORDS: &[&str] = &["resume", "unpause", "continue", "fortsett", "gjenoppta"];
const ADD_WORDS: &[&str] = &["add", "extend", "extra", "more", "legg", "forleng", "forlenge"];
const START_WORDS: &[&str] = &["set", "start", "make", "create", "new", "put", "sett", "lag", "starte", "ny", "still"];
/// Words before a request that say nothing about it.
const FILLER: &[&str] = &["please", "hey", "okay", "ok", "so", "oh", "well", "um", "uh", "jarvis", "hei", "ja", "jo"];
/// Moving the alarm that is set: "change my alarm to 7", "push my alarm back 10 minutes", "add 10
/// minutes to my alarm", "flytt vekkerklokka til 8".
const MOVE_WORDS: &[&str] = &[
    "change", "move", "reset", "update", "push", "delay", "postpone", "add", "extend", "endre", "flytt", "flytte",
    "utsett", "legg", "forleng",
];
/// Which way a move by a while goes: "10 minutes later", "back 10 minutes"; "earlier", "forward".
const LATER_WORDS: &[&str] =
    &["later", "back", "delay", "postpone", "add", "extend", "senere", "seinere", "utsett", "legg", "forleng"];
const EARLIER_WORDS: &[&str] = &["earlier", "forward", "sooner", "tidligere", "fram", "frem"];
/// Before "alarm": the one that is set ("my alarm", "the alarm"), or a new one ("an extra alarm",
/// "one more alarm", "a later alarm").
const THE_ALARM: &[&str] = &["my", "the", "this", "that", "min", "mi", "mitt"];
const NEW_ALARM: &[&str] =
    &["a", "an", "one", "another", "extra", "more", "new", "second", "en", "ei", "et", "ny", "nytt"];
/// Norwegian definite forms are the alarm that is set by themselves: "flytt vekkerklokka".
const ALARM_DEFINITE: &[&str] = &["vekkerklokka", "vekkerklokken", "vekkingen"];
const LEFT_WORDS: &[&str] = &["left", "remaining", "remain", "igjen", "gjenstår"];
const QUESTION_WORDS: &[&str] = &[
    "is", "are", "when", "when's", "whens", "what", "what's", "whats", "how", "how's", "do", "have", "check", "har",
    "når", "er", "hva", "hvilken", "hvor", "hvordan", "sjekk",
];
/// Words right before "timer" that are not its name.
const NOT_NAMES: &[&str] = &[
    "a", "an", "the", "my", "this", "that", "one", "another", "new", "any", "all", "every", "of", "for", "on", "set",
    "start", "cancel", "stop", "pause", "resume", "delete", "remove", "clear", "add", "is", "your", "en", "ei", "et",
    "ett", "den", "det", "min", "mi", "mitt", "ny", "alle", "på", "til", "sett", "lag", "avbryt", "stopp", "slett",
    "fjern", "ingen", "noen", "hvilke", "which", "what",
];

/// Lower-case words with digits kept ("1.5", "17:30"), hyphens split ("10-minute").
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == '-')
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, '.' | ',' | '\'' | ':'))
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

fn is_timer_word(w: &str) -> bool {
    TIMER_WORDS.contains(&w) || compound_name(w).is_some()
}

/// "pastatimer", "pastatimeren": the part before "timer".
fn compound_name(w: &str) -> Option<&str> {
    let stem = w.strip_suffix("timeren").or_else(|| w.strip_suffix("timer"))?;
    (stem.chars().count() >= 3).then_some(stem)
}

pub fn parse(text: &str) -> Option<TimerCommand> {
    let words = tokens(text);
    let has = |w: &str| words.iter().any(|x| x == w);
    let any = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    let remind = words.windows(2).any(|p| matches!((p[0].as_str(), p[1].as_str()), ("remind", "me") | ("minn", "meg")));
    if remind {
        let (when, used) = when_words(&words, AT_WORDS);
        return Some(TimerCommand::Remind { when, message: reminder_text(&words, &used) });
    }
    if wake_me(&words) || any(ALARM_WORDS) || any(SNOOZE_WORDS) {
        return alarm(&words);
    }
    let (seconds, used) = duration_words(&words);
    let timer = words.iter().any(|w| is_timer_word(w));
    let reminder = any(REMINDER_WORDS);
    if !timer && !reminder {
        // "How much time is left?", "hvor lang tid er det igjen?"
        if any(LEFT_WORDS) && ["time", "long", "tid", "lenge"].iter().any(|w| has(w)) {
            return Some(TimerCommand::Remaining(Which::default()));
        }
        return None;
    }
    let plural = ["timers", "timerne", "reminders", "påminnelsene"].iter().any(|w| has(w));
    let which = Which {
        name: timer_name(&words, &used),
        all: has("all") || has("alle") || has("every") || has("everything"),
        reminder,
        ..Which::default()
    };
    let with_length = Which { seconds, ..which.clone() };
    let which_all = Which { all: which.all || plural, ..with_length.clone() };
    if any(CANCEL_WORDS) {
        return Some(TimerCommand::Cancel(which_all));
    }
    if any(PAUSE_WORDS) {
        return Some(TimerCommand::Pause(which_all));
    }
    if any(RESUME_WORDS) {
        return Some(TimerCommand::Resume(which_all));
    }
    if let Some(seconds) = seconds.filter(|_| any(ADD_WORDS)) {
        return Some(TimerCommand::Add { seconds, which });
    }
    if seconds.is_some() || any(START_WORDS) {
        return Some(TimerCommand::Start { seconds, name: which.name });
    }
    Some(TimerCommand::Remaining(with_length))
}

/// "Wake me up", "vekk meg".
fn wake_me(words: &[String]) -> bool {
    words
        .windows(2)
        .any(|p| matches!((p[0].as_str(), p[1].as_str()), ("wake", "me" | "us") | ("vekk" | "vekke", "meg" | "oss")))
}

/// The house's alarm system rather than a wake-up alarm: arming it, "the house alarm", and "is the
/// alarm on?" (but "is my alarm set?" is the wake-up alarm).
fn house_alarm(words: &[String]) -> bool {
    let has = |w: &str| words.iter().any(|x| x == w);
    let text = format!(" {} ", words.join(" "));
    // Only the bare word "alarm", as in house.rs: "is the alarm clock set", "is there an alarm set"
    // and "er vekkerklokka stilt" are the wake-up alarm.
    let status = first_word(words).is_some_and(|w| w == "is" || w == "er")
        && has("alarm")
        && !["my", "min", "time", "tomorrow", "morgen", "wake", "clock", "there"].iter().any(|w| has(w))
        && clock_words(words, ALARM_AT_WORDS, false).is_none();
    words.iter().any(|w| HOUSE_ALARM.contains(&w.as_str()))
        || ["house alarm", "home alarm", "alarm system", "burglar alarm"]
            .iter()
            .any(|p| text.contains(&format!(" {p} ")))
        || status
}

/// A wake-up alarm request; `None` when it is the house's alarm system.
fn alarm(words: &[String]) -> Option<TimerCommand> {
    let any = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    let has = |w: &str| words.iter().any(|x| x == w);
    if any(SNOOZE_WORDS) {
        return Some(TimerCommand::Snooze);
    }
    let wake = wake_me(words);
    if !wake && house_alarm(words) {
        return None;
    }
    let which = Which { alarm: true, all: has("all") || has("alle"), ..Which::default() };
    if !wake && first_word(words).is_some_and(|w| QUESTION_WORDS.contains(&w)) {
        return Some(TimerCommand::Remaining(which));
    }
    if any(&["cancel", "delete", "remove", "clear", "avbryt", "slett", "fjern"]) {
        return Some(TimerCommand::Cancel(which));
    }
    if any(&["stop", "off", "dismiss", "silence", "stopp", "av"]) {
        return Some(TimerCommand::StopAlarm);
    }
    let (when, _) = when_words(words, ALARM_AT_WORDS);
    // "Push my alarm back 10 minutes", "change my alarm to 7": the one that is set, not a second one.
    // "Add an extra alarm in 10 minutes" and "set the alarm for 7" are a new one.
    if !wake && any(MOVE_WORDS) && the_alarm(words) {
        let shift = |seconds: u32| {
            if any(EARLIER_WORDS) {
                Some(-i64::from(seconds))
            } else {
                any(LATER_WORDS).then_some(i64::from(seconds))
            }
        };
        match when {
            Some(When::At(clock)) => return Some(TimerCommand::MoveAlarm(Move::To(When::At(clock)))),
            // "Change my alarm by 15 minutes": earlier or later is not said, and now plus 15 minutes
            // is surely not meant.
            Some(When::In(seconds)) => return shift(seconds).map(|by| TimerCommand::MoveAlarm(Move::By(by))),
            None => {}
        }
    }
    if wake || when.is_some() || any(START_WORDS) || any(&["need", "want", "trenger"]) {
        return Some(TimerCommand::Alarm { when });
    }
    // "What alarms do I have" is a question above; "show my alarms", "my alarm" and "alarm status"
    // ask too. A lone "alarm" or "the alarm" is not a whole request yet: the rest may still come.
    if any(&["alarms", "vekkerklokkene", "show", "list", "next", "status", "vis", "neste", "my", "min", "mi"]) {
        return Some(TimerCommand::Remaining(which));
    }
    None
}

/// The alarm that is set is meant ("my alarm", "the alarm", "vekkerklokka"), not a new one ("an
/// extra alarm", "one more alarm", "a later alarm").
fn the_alarm(words: &[String]) -> bool {
    let Some(at) = words.iter().position(|w| ALARM_WORDS.contains(&w.as_str())) else { return false };
    let before = &words[at.saturating_sub(2)..at];
    if before.iter().any(|w| NEW_ALARM.contains(&w.as_str())) {
        return false;
    }
    ALARM_DEFINITE.contains(&words[at].as_str()) || before.iter().any(|w| THE_ALARM.contains(&w.as_str()))
}

/// The first word that is not a greeting or a filler: "okay, is my alarm set?".
fn first_word(words: &[String]) -> Option<&str> {
    words.iter().map(String::as_str).find(|w| !FILLER.contains(w))
}

/// The duration a request names, in seconds; `None` when it names none. For answers to "For how
/// long?".
pub fn duration(text: &str) -> Option<u32> {
    duration_words(&tokens(text)).0
}

/// A time of day or a duration, for answers to "When should I remind you?": "at 5", "5 pm",
/// "half past six", "in an hour".
pub fn when(text: &str) -> Option<When> {
    let words = tokens(text);
    pick(&words, clock_words(&words, AT_WORDS, true)).0
}

/// A time of day after one of `markers`, else a duration; and which words it used.
fn when_words(words: &[String], markers: &[&str]) -> (Option<When>, Vec<bool>) {
    pick(words, clock_words(words, markers, false))
}

/// The time of day, unless a duration is said first, with "in" before it: "in 10 minutes to call
/// Anna at 3" is in 10 minutes, and "in an hour and 15" is not 15:00, but "at 6 to leave in 20
/// minutes" is at 6. Without "in" the time wins, since "a quarter to eight" also reads as a quarter
/// of an hour.
fn pick(words: &[String], clock: Option<(Clock, Vec<bool>)>) -> (Option<When>, Vec<bool>) {
    let (seconds, by_length) = duration_words(words);
    let Some((clock, by_clock)) = clock else { return (seconds.map(When::In), by_length) };
    let Some(seconds) = seconds else { return (Some(When::At(clock)), by_clock) };
    let first = by_length.iter().position(|&u| u).unwrap_or(0);
    let clock_first = by_clock.iter().position(|&u| u).unwrap_or(0);
    if first > 0 && first < clock_first && ["in", "om"].contains(&words[first - 1].as_str()) {
        let mut used = by_length;
        used[first - 1] = true;
        return (Some(When::In(seconds)), used);
    }
    (Some(When::At(clock)), by_clock)
}

/// A time of day: after one of `markers`, or anywhere when `bare`. With "tomorrow" and "in the
/// evening" wherever they are, and which words it used.
fn clock_words(words: &[String], markers: &[&str], bare: bool) -> Option<(Clock, Vec<bool>)> {
    let word = |k: usize| words.get(k).map_or("", String::as_str);
    let starts = (0..words.len()).filter(|&i| bare || i > 0 && markers.contains(&word(i - 1)));
    let (start, (hour, minute, end, mut pm)) =
        starts.into_iter().find_map(|i| time_at(words, i).filter(|&t| !counts_at(words, i, t)).map(|t| (i, t)))?;
    let mut used = vec![false; words.len()];
    used[start..end].fill(true);
    if start > 0 && markers.contains(&word(start - 1)) {
        used[start - 1] = true;
    }
    let mut tomorrow = false;
    for i in 0..words.len() {
        match word(i) {
            "tomorrow" | "imorgen" => {
                tomorrow = true;
                used[i] = true;
            }
            "i" if word(i + 1) == "morgen" => {
                tomorrow = true;
                used[i..i + 2].fill(true);
            }
            _ => {}
        }
        let part = match word(i) {
            "morning" | "morgenen" | "formiddagen" => Some(false),
            "afternoon" | "evening" | "tonight" | "ettermiddagen" | "ettermiddag" | "kveld" | "kvelden" => Some(true),
            _ => None,
        };
        if let Some(part) = part {
            pm = pm.or(Some(part));
            used[i] = true;
            // "in the evening", "this morning", "om kvelden", "i kveld"
            let mut k = i;
            while k > 0 && ["in", "the", "this", "om", "i"].contains(&word(k - 1)) {
                k -= 1;
                used[k] = true;
            }
        }
    }
    let pm = pm.filter(|_| (1..=12).contains(&hour));
    Some((Clock { hour, minute, pm, tomorrow }, used))
}

/// A time of day starting at `i`: the hour, minute, the index after it, and am or pm if said.
fn time_at(words: &[String], i: usize) -> Option<(u8, u8, usize, Option<bool>)> {
    let word = |k: usize| words.get(k).map_or("", String::as_str);
    // Norwegian "halv åtte" is half past seven.
    if word(i) == "halv" {
        if let Some((h, 0, end, pm)) = hour_at(words, i + 1) {
            return Some((hour_before(h), 30, end, pm));
        }
    }
    // "half past six", "quarter to eight", "ten past six", "kvart over sju", "ti på åtte"
    if let Some((minutes, k)) = minutes_said(words, i) {
        let before = match word(k) {
            "past" | "after" | "over" => Some(false),
            "to" | "before" | "på" => Some(true),
            _ => None,
        };
        // "Fem over halv åtte" is 07:35, "fem på halv åtte" 07:25.
        if let (Some(before), "halv") = (before, word(k + 1)) {
            if let Some((h, 0, end, pm)) = hour_at(words, k + 2) {
                let hour = hour_before(h);
                return Some(if before {
                    (hour, 30 - minutes.min(30), end, pm)
                } else {
                    (hour, 30 + minutes.min(29), end, pm)
                });
            }
        }
        if let (Some(before), Some((h, 0, end, pm))) = (before, hour_at(words, k + 1)) {
            return Some(if before { (hour_before(h), 60 - minutes, end, pm) } else { (h, minutes, end, pm) });
        }
    }
    hour_at(words, i)
}

/// The hour before `h` on a 12-hour clock when `h` is one: "halv ett" is 12:30, either half of the
/// day, not a fixed 00:30.
fn hour_before(h: u8) -> u8 {
    match h {
        0 => 23,
        1 => 12,
        h => h - 1,
    }
}

/// "Look at 2 houses", "buy 3 eggs": a bare number right before a counted noun is not a time of
/// day. After "at" or "klokka" alone it still is: "remind me at 5 people are coming".
fn counts_at(words: &[String], i: usize, (_, minute, end, pm): (u8, u8, usize, Option<bool>)) -> bool {
    let word = |k: usize| if k < words.len() { words[k].as_str() } else { "" };
    let counted = minute == 0 && pm.is_none() && end == i + 1 && counts_things(word(end));
    let marker = i > 0 && ["at", "klokka", "klokken", "kl"].contains(&word(i - 1));
    let verb = i > 1 && ["look", "looking", "se", "ser", "kikke"].contains(&word(i - 2));
    counted && (!marker || verb)
}

/// A noun that a number right before it counts: "2 houses", "3 boxes". A short list on purpose:
/// "at 6 thanks", "at 7 weekdays" keep their time.
fn counts_things(word: &str) -> bool {
    const COUNTED: &[&str] = &[
        "houses",
        "flats",
        "apartments",
        "things",
        "people",
        "persons",
        "items",
        "times",
        "kids",
        "children",
        "boxes",
        "bags",
        "bottles",
        "pills",
        "tablets",
        "eggs",
        "pages",
        "emails",
        "calls",
        "letters",
        "packages",
        "parcels",
        "hus",
        "leiligheter",
        "ting",
        "personer",
        "folk",
        "barn",
        "esker",
        "poser",
        "flasker",
        "piller",
        "egg",
        "sider",
    ];
    COUNTED.contains(&word)
}

/// The minutes of "half past", "a quarter to", "ten past": their value and the index after them.
fn minutes_said(words: &[String], i: usize) -> Option<(u8, usize)> {
    let word = |k: usize| words.get(k).map_or("", String::as_str);
    match word(i) {
        "half" => Some((30, i + 1)),
        "quarter" | "kvart" => Some((15, i + 1)),
        "a" if word(i + 1) == "quarter" => Some((15, i + 2)),
        _ => {
            let (n, k) = number(words, i).filter(|(n, _)| n.fract() == 0.0 && (1.0..30.0).contains(n))?;
            let k = k + usize::from(matches!(word(k), "minutes" | "minute" | "minutter" | "min"));
            Some((n as u8, k))
        }
    }
}

/// An hour, with its minutes if said: "5", "17:30", "6.30", "5pm", "seven thirty", "five o'clock".
fn hour_at(words: &[String], i: usize) -> Option<(u8, u8, usize, Option<bool>)> {
    let word = |k: usize| words.get(k).map_or("", String::as_str);
    let w = words.get(i)?.as_str();
    let (body, pm) = match meridiem_suffix(w) {
        Some((body, pm)) => (body, Some(pm)),
        None => (w, None),
    };
    if body.starts_with(|c: char| c.is_ascii_digit()) {
        let (hour, minute) = match body.split_once([':', '.']) {
            Some((h, m)) if m.len() == 2 => (h.parse().ok()?, m.parse().ok()?),
            Some(_) => return None,
            None => (body.parse().ok()?, 0),
        };
        return finish_time(words, hour, minute, i + 1, pm);
    }
    // "A minute" is a duration, not one o'clock.
    if matches!(w, "a" | "an") {
        return None;
    }
    let (n, mut k) = number(words, i).filter(|(n, _)| n.fract() == 0.0)?;
    let mut minute = 0;
    // "seven oh five", "seven thirty", "six forty five"
    if matches!(word(k), "oh" | "o") {
        if let Some((m, end)) = number(words, k + 1).filter(|(m, _)| m.fract() == 0.0 && (1.0..10.0).contains(m)) {
            (minute, k) = (m as u8, end);
        }
    } else if let Some((m, end)) = (k < words.len())
        .then(|| number(words, k))
        .flatten()
        .filter(|(m, end)| m.fract() == 0.0 && (10.0..60.0).contains(m) && unit(word(*end), "x", 2.0).is_none())
    {
        (minute, k) = (m as u8, end);
    }
    finish_time(words, n as u8, minute, k, None)
}

fn finish_time(
    words: &[String],
    hour: u8,
    minute: u8,
    mut k: usize,
    mut pm: Option<bool>,
) -> Option<(u8, u8, usize, Option<bool>)> {
    let word = |k: usize| words.get(k).map_or("", String::as_str);
    if hour > 23 || minute > 59 {
        return None;
    }
    if matches!(word(k), "o'clock" | "oclock") {
        k += 1;
    }
    if pm.is_none() {
        if let Some(said) = meridiem(word(k)) {
            pm = Some(said);
            k += 1;
        }
    }
    // "For 10 minutes" is a duration.
    if unit(word(k), "x", 2.0).is_some() {
        return None;
    }
    Some((hour, minute, k, pm))
}

/// "pm", "p.m": whether it is the afternoon.
fn meridiem(w: &str) -> Option<bool> {
    match w {
        "am" | "a.m" => Some(false),
        "pm" | "p.m" => Some(true),
        _ => None,
    }
}

/// "5pm", "7:30am": the time and whether it is the afternoon.
fn meridiem_suffix(w: &str) -> Option<(&str, bool)> {
    ["a.m", "p.m", "am", "pm"].iter().find_map(|suffix| {
        let body = w.strip_suffix(suffix)?;
        body.starts_with(|c: char| c.is_ascii_digit()).then(|| (body, suffix.starts_with('p')))
    })
}

/// The duration in the words, and which words it used.
fn duration_words(words: &[String]) -> (Option<u32>, Vec<bool>) {
    let mut used = vec![false; words.len()];
    let mut total = 0.0;
    let mut i = 0;
    while i < words.len() {
        // A self-standing unit: "en halvtime" counts its article as the number.
        if let Some(unit) = bare_unit(&words[i]) {
            total += unit;
            used[i] = true;
            if i > 0 && word_number(&words[i - 1]) == Some(1) {
                used[i - 1] = true;
            }
            i += 1;
            continue;
        }
        let Some((value, j)) = number(words, i) else {
            i += 1;
            continue;
        };
        let Some(unit) = words.get(j).and_then(|w| unit(w, &words[i], value)) else {
            i += 1;
            continue;
        };
        let mut end = j + 1;
        let mut amount = value * unit;
        // "an hour and a half", "en time og en halv"
        let halved = half_follows(words, end);
        if halved {
            amount += unit / 2.0;
            end += 3;
        }
        // "1 hour 30", "an hour and fifteen": minutes without their unit. Digits or a word from
        // ten up, since Norwegian "to" is two.
        if unit == 3600.0 && !halved {
            let at = end + usize::from(words.get(end).is_some_and(|w| w == "and" || w == "og"));
            let minutes = (at < words.len()).then(|| number(words, at)).flatten().filter(|&(m, k)| {
                let digits = words[at].starts_with(|c: char| c.is_ascii_digit());
                m.fract() == 0.0
                    && (1.0..60.0).contains(&m)
                    && (digits || m >= 10.0)
                    && words.get(k).is_none_or(|w| self::unit(w, &words[at], m).is_none())
            });
            if let Some((m, k)) = minutes {
                amount += m * 60.0;
                end = k;
            }
        }
        // "a quarter of an hour", "three quarters of an hour"
        if unit == 900.0 {
            if words
                .get(end..end + 3)
                .is_some_and(|w| w[0] == "of" && matches!(w[1].as_str(), "an" | "a") && w[2] == "hour")
            {
                end += 3;
            } else if words.get(end).is_some_and(|w| w == "hour") {
                end += 1;
            }
        }
        total += amount;
        used[i..end].fill(true);
        i = end;
    }
    let seconds = total.round() as u32;
    ((seconds > 0).then_some(seconds), used)
}

fn half_follows(words: &[String], at: usize) -> bool {
    words.get(at..at + 3).is_some_and(|w| {
        matches!((w[0].as_str(), w[1].as_str(), w[2].as_str()), ("and", "a", "half") | ("og", "en" | "ei", "halv"))
    })
}

/// A number at `i`: digits, number words ("twenty five", "tjuefem"), "half", "and a half". The
/// value and the index after it.
fn number(words: &[String], i: usize) -> Option<(f64, usize)> {
    let w = words[i].as_str();
    let (mut value, mut j) = if let Ok(n) = w.replace(',', ".").parse::<f64>() {
        (n, i + 1)
    } else if let Some(n) = word_number(w) {
        let mut n = n;
        let mut j = i + 1;
        if (20..100).contains(&n) && n % 10 == 0 {
            // English "twenty to eight" is not Norwegian "tjue to".
            let english = TENS[..8].iter().any(|(tens, _)| *tens == w);
            let ones = words.get(j).filter(|o| !(english && o.as_str() == "to")).and_then(|o| word_number(o));
            if let Some(ones) = ones.filter(|o| (1..10).contains(o)) {
                n += ones;
                j += 1;
            }
        }
        (n as f64, j)
    } else if w == "half" || w == "halv" {
        // "half an hour", "en halv time"
        let skip = words.get(i + 1).is_some_and(|w| matches!(w.as_str(), "a" | "an"));
        (0.5, i + 1 + skip as usize)
    } else if w == "halvannen" {
        (1.5, i + 1)
    } else {
        return None;
    };
    // "one and a half hours", "en og en halv time"
    if half_follows(words, j) {
        value += 0.5;
        j += 3;
    }
    Some((value, j))
}

const ONES_EN: &[&str] = &[
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const ONES_NO: &[&str] = &[
    "null", "en", "to", "tre", "fire", "fem", "seks", "sju", "åtte", "ni", "ti", "elleve", "tolv", "tretten",
    "fjorten", "femten", "seksten", "sytten", "atten", "nitten",
];
const TENS: &[(&str, u32)] = &[
    ("twenty", 20),
    ("thirty", 30),
    ("forty", 40),
    ("fifty", 50),
    ("sixty", 60),
    ("seventy", 70),
    ("eighty", 80),
    ("ninety", 90),
    ("tjue", 20),
    ("tretti", 30),
    ("førti", 40),
    ("femti", 50),
    ("seksti", 60),
    ("sytti", 70),
    ("åtti", 80),
    ("nitti", 90),
];

fn word_number(w: &str) -> Option<u32> {
    match w {
        "a" | "an" | "ei" | "ett" | "et" | "én" => return Some(1),
        "syv" => return Some(7),
        _ => {}
    }
    if let Some(n) = ONES_EN.iter().position(|x| *x == w).or_else(|| ONES_NO.iter().position(|x| *x == w)) {
        return Some(n as u32);
    }
    // "twenty", "tjue", and Norwegian one-word compounds: "tjuefem", "trettito".
    TENS.iter().find_map(|&(tens, n)| {
        let rest = w.strip_prefix(tens)?;
        if rest.is_empty() {
            return Some(n);
        }
        let ones = ONES_NO.iter().chain(ONES_EN).position(|x| *x == rest).map(|p| p % 20)?;
        (1..10).contains(&ones).then_some(n + ones as u32)
    })
}

/// Seconds per unit. `number_word` and `value` rule out "a timer" (Norwegian "timer" is also
/// "hours") and "a time".
fn unit(w: &str, number_word: &str, value: f64) -> Option<f64> {
    let english_article = matches!(number_word, "a" | "an" | "one");
    Some(match w {
        "second" | "seconds" | "sec" | "secs" | "sekund" | "sekunder" | "sekundet" => 1.0,
        "minute" | "minutes" | "min" | "mins" | "minutt" | "minutter" | "minuttet" => 60.0,
        "hour" | "hours" | "hr" | "hrs" | "h" => 3600.0,
        // Norwegian: "en time", "1,5 time", "to timer".
        "time" | "timen" if !english_article => 3600.0,
        "timer" if !english_article && value != 1.0 && !matches!(number_word, "en" | "ei" | "ett" | "et") => 3600.0,
        "quarter" | "quarters" | "kvarter" | "kvarteret" => 900.0,
        "halvtime" | "halvtimen" | "halvtimes" => 1800.0,
        _ => return None,
    })
}

/// Units that need no number: "halvtime" is a half hour.
fn bare_unit(w: &str) -> Option<f64> {
    matches!(w, "halvtime" | "halvtimen").then_some(1800.0)
}

fn timer_name(words: &[String], used: &[bool]) -> Option<String> {
    let is_free = |i: usize| !used[i] && number(words, i).is_none() && !is_timer_word(&words[i]);
    let stop = |w: &str| matches!(w, "for" | "please" | "på" | "takk" | "now" | "nå" | "and" | "og" | "me" | "meg");
    let name_from = |i: usize| -> Option<String> {
        let name: Vec<&str> =
            (i..words.len()).take_while(|&k| is_free(k) && !stop(&words[k])).map(|k| words[k].as_str()).collect();
        (!name.is_empty()).then(|| name.join(" "))
    };
    // "a timer called pasta", "en timer som heter pasta"
    if let Some(i) = words.iter().position(|w| matches!(w.as_str(), "called" | "named" | "heter" | "kalt")) {
        if let Some(name) = name_from(i + 1) {
            return Some(name);
        }
    }
    let first = words.iter().position(|w| is_timer_word(w))?;
    if let Some(stem) = compound_name(&words[first]) {
        return Some(stem.to_owned());
    }
    if let Some(before) = first.checked_sub(1) {
        if is_free(before) && !NOT_NAMES.contains(&words[before].as_str()) {
            return Some(words[before].clone());
        }
    }
    // "set a timer for 10 minutes for the pasta", "a 2 minute timer for the eggs"
    (first + 1..words.len()).filter(|&i| matches!(words[i].as_str(), "for" | "til")).find_map(|i| {
        let start = i + 1 + usize::from(words.get(i + 1).is_some_and(|w| matches!(w.as_str(), "the" | "my" | "our")));
        (start < words.len()).then(|| name_from(start)).flatten()
    })
}

/// The message of "remind me in 10 minutes to check the oven", with the person turned around:
/// "check the oven", "call your mother".
fn reminder_text(words: &[String], used: &[bool]) -> Option<String> {
    let mut keep: Vec<bool> = used.iter().map(|u| !u).collect();
    for (i, w) in words.iter().enumerate() {
        let next_used = used.get(i + 1).copied().unwrap_or(false);
        // "in 10 minutes", "om ti minutter", "after an hour"
        if next_used && matches!(w.as_str(), "in" | "om" | "after" | "etter" | "for") {
            keep[i] = false;
        }
        if matches!(w.as_str(), "remind" | "minn") && words.get(i + 1).is_some_and(|n| n == "me" || n == "meg") {
            keep[i] = false;
            keep[i + 1] = false;
        }
    }
    let mut rest: Vec<&str> = (0..words.len()).filter(|&i| keep[i]).map(|i| words[i].as_str()).collect();
    while rest.first().is_some_and(|w| {
        matches!(*w, "to" | "that" | "about" | "på" | "å" | "om" | "can" | "you" | "please" | "kan" | "du")
    }) {
        rest.remove(0);
    }
    while rest.last().is_some_and(|w| matches!(*w, "please" | "takk" | "from" | "now" | "nå")) {
        rest.pop();
    }
    // "Remind me about it at 5": nothing to say but that it is a reminder.
    if matches!(rest.as_slice(), ["it" | "this" | "that" | "det" | "dette"]) {
        return None;
    }
    let swapped: Vec<&str> = rest
        .iter()
        .map(|w| match *w {
            "my" => "your",
            "me" => "you",
            "i" | "i'm" => "you",
            "myself" => "yourself",
            "min" => "din",
            "mi" => "di",
            "mitt" => "ditt",
            "mine" => "dine",
            "meg" => "deg",
            "jeg" => "du",
            other => other,
        })
        .collect();
    (!swapped.is_empty()).then(|| swapped.join(" "))
}

// --- Running timers ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running { ends: Instant },
    Paused { left: Duration },
}

#[derive(Debug, Clone)]
pub struct Timer {
    pub id: u32,
    pub name: Option<String>,
    pub reminder: Option<String>,
    /// The language it was set in, for the words at the end.
    pub lang: Lang,
    pub total: Duration,
    pub state: State,
    /// A reminder without a message still says it is one.
    pub is_reminder: bool,
    /// A wake-up alarm: it rings, and "snooze" sets it again.
    pub is_alarm: bool,
    /// An alarm's time of day, "07:00", for its name.
    pub at: Option<String>,
    /// It ended while the engine was down, at this time of day ("14:30"): it is told late.
    pub late: Option<String>,
}

impl Timer {
    pub fn left(&self, now: Instant) -> Duration {
        match self.state {
            State::Running { ends } => ends.saturating_duration_since(now),
            State::Paused { left } => left,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, State::Running { .. })
    }

    /// "the pasta timer", "the 10 minute timer", "the reminder to call mom", "the 07:00 alarm";
    /// Norwegian "pasta-timeren", "timeren på 10 minutter", "påminnelsen", "vekkerklokka på 07:00".
    pub fn label(&self, lang: Lang) -> String {
        let total = self.total.as_secs() as u32;
        if self.is_alarm {
            return match (lang, &self.at) {
                (Lang::English, Some(at)) => format!("the {at} alarm"),
                (Lang::English, None) => "the alarm".into(),
                (Lang::Norwegian, Some(at)) => format!("vekkerklokka på {at}"),
                (Lang::Norwegian, None) => "vekkerklokka".into(),
            };
        }
        match (lang, &self.name, &self.reminder, self.is_reminder) {
            (Lang::English, _, Some(message), _) => format!("the reminder to {message}"),
            (Lang::English, _, None, true) => "the reminder".into(),
            (Lang::English, Some(name), ..) => format!("the {name} timer"),
            (Lang::English, None, ..) => match single_unit(total) {
                Some((n, unit)) => format!("the {n} {unit} timer"),
                None => format!("the timer for {}", say_duration(total, lang)),
            },
            (Lang::Norwegian, _, _, true) => "påminnelsen".into(),
            (Lang::Norwegian, Some(name), ..) => format!("{name}-timeren"),
            (Lang::Norwegian, None, ..) => format!("timeren på {}", say_duration(total, lang)),
        }
    }

    fn matches(&self, which: &Which) -> bool {
        if which.alarm != self.is_alarm || which.reminder && !self.is_reminder {
            return false;
        }
        if let Some(seconds) = which.seconds {
            if self.total.as_secs() as u32 != seconds {
                return false;
            }
        }
        match &which.name {
            None => true,
            Some(name) => {
                let hay = [self.name.as_deref(), self.reminder.as_deref()];
                hay.iter().flatten().any(|h| h.contains(name.as_str()) || name.contains(*h))
            }
        }
    }
}

/// What a satellite with its own timer display needs (ESPHome timer events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub id: String,
    pub name: String,
    pub total_seconds: u32,
    pub seconds_left: u32,
    pub active: bool,
    /// Reminders end with speech, not a ring.
    pub reminder: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Started,
    Updated,
    Cancelled,
    Finished,
}

impl Notice {
    fn of(timer: &Timer, kind: NoticeKind, now: Instant) -> Self {
        Self {
            kind,
            id: format!("timer-{}", timer.id),
            name: timer.name.clone().unwrap_or_default(),
            total_seconds: timer.total.as_secs() as u32,
            seconds_left: timer.left(now).as_secs_f32().ceil() as u32,
            active: timer.is_running() && kind != NoticeKind::Finished,
            reminder: timer.is_reminder,
        }
    }
}

/// What a new timer is: also a start that is waiting for its duration or time ("Set a timer."
/// "For how long?").
#[derive(Debug, Clone, Default)]
struct Plan {
    name: Option<String>,
    reminder: Option<String>,
    is_reminder: bool,
    is_alarm: bool,
}

/// A timer as kept on disk: wall-clock times, since an `Instant` means nothing after a restart.
#[derive(Debug, Serialize, Deserialize)]
struct Kept {
    id: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    reminder: Option<String>,
    #[serde(default)]
    norwegian: bool,
    total_seconds: u64,
    /// When a running one ends.
    #[serde(default)]
    ends: Option<Timestamp>,
    /// What a paused one has left.
    #[serde(default)]
    left_seconds: Option<u64>,
    #[serde(default)]
    is_reminder: bool,
    #[serde(default)]
    is_alarm: bool,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    late: Option<String>,
}

/// The kept timers in `text`, one by one: an entry that does not read (an older or a damaged
/// file) loses only itself, not every alarm. A file that is not a list at all is set aside
/// rather than overwritten by the next save.
fn read_kept(text: &str, path: &std::path::Path) -> Vec<Kept> {
    let entries: Vec<serde_json::Value> = match serde_json::from_str(text) {
        Ok(entries) => entries,
        Err(error) => {
            warn!(%error, path = %path.display(), "could not read the kept timers; set aside");
            let _ = std::fs::rename(path, path.with_extension("bad"));
            return Vec::new();
        }
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            serde_json::from_value(entry).inspect_err(|error| warn!(%error, "a kept timer did not read; dropped")).ok()
        })
        .collect()
}

#[derive(Default)]
pub struct Timers {
    list: Vec<Timer>,
    next_id: u32,
    pending: Option<Plan>,
    /// After "Which one?": the change asked for, and the timers named in the question, in the
    /// order they were said.
    choosing: Option<Choosing>,
    /// The alarm that went off last and when, for "snooze" and "stop the alarm".
    rang: Option<(Timer, Instant)>,
    /// The file they are kept in across restarts.
    store: Option<PathBuf>,
    /// A fixed clock, for tests: this instant was this local time.
    clock: Option<(Instant, Zoned)>,
    /// Ended while the engine was down and dropped: a device may still show them.
    dropped: Vec<Notice>,
}

impl Timers {
    /// Kept in `path`, and read back from it: timers still running go on, those that ended while
    /// the engine was down are told late (see `due`) or dropped.
    pub fn kept_in(path: PathBuf) -> Self {
        Self::restore(path, Instant::now(), None)
    }

    fn restore(path: PathBuf, now: Instant, clock: Option<(Instant, Zoned)>) -> Self {
        let mut timers = Self { store: Some(path.clone()), clock, ..Self::default() };
        let kept = match std::fs::read_to_string(&path) {
            Ok(text) => read_kept(&text, &path),
            Err(_) => Vec::new(),
        };
        let local = timers.local(now);
        for k in kept {
            timers.next_id = timers.next_id.max(k.id);
            let mut timer = Timer {
                id: k.id,
                name: k.name,
                reminder: k.reminder,
                lang: if k.norwegian { Lang::Norwegian } else { Lang::English },
                total: Duration::from_secs(k.total_seconds),
                state: State::Paused { left: Duration::from_secs(k.left_seconds.unwrap_or(0)) },
                is_reminder: k.is_reminder,
                is_alarm: k.is_alarm,
                at: k.at,
                late: k.late,
            };
            if let Some(ends) = k.ends {
                let left = ends.duration_since(local.timestamp());
                if left.is_positive() && timer.late.is_none() {
                    timer.state = State::Running { ends: now + left.unsigned_abs() };
                } else {
                    let late = left.unsigned_abs();
                    let limit = if timer.is_alarm { LATE_ALARM } else { LATE_TIMER };
                    if !timer.is_reminder && timer.late.is_none() && late > limit {
                        info!(label = timer.label(Lang::English), late = ?late, "ended while down; dropped");
                        timers.dropped.push(Notice::of(&timer, NoticeKind::Cancelled, now));
                        continue;
                    }
                    let due = ends.to_zoned(local.time_zone().clone()).strftime("%H:%M").to_string();
                    timer.late = timer.late.or(Some(due));
                    timer.state = State::Running { ends: now + LATE_WAIT };
                }
            }
            timers.list.push(timer);
        }
        if !timers.list.is_empty() {
            info!(count = timers.list.len(), path = %path.display(), "timers restored");
        }
        timers
    }

    /// The same timers on a fixed clock: `at` is the local time `local`.
    pub fn with_clock(mut self, at: Instant, local: Zoned) -> Self {
        self.clock = Some((at, local));
        self
    }

    /// Local time at `at`.
    fn local(&self, at: Instant) -> Zoned {
        let (base, wall) = match &self.clock {
            Some((base, wall)) => (*base, wall.clone()),
            None => (Instant::now(), Zoned::now()),
        };
        let offset = match at.checked_duration_since(base) {
            Some(ahead) => SignedDuration::try_from(ahead).unwrap_or_default(),
            None => -SignedDuration::try_from(base - at).unwrap_or_default(),
        };
        wall.checked_add(offset).unwrap_or(wall)
    }

    fn save(&self, now: Instant) {
        let Some(path) = &self.store else { return };
        let wall = self.local(now).timestamp();
        let kept: Vec<Kept> = self
            .list
            .iter()
            .map(|t| Kept {
                id: t.id,
                name: t.name.clone(),
                reminder: t.reminder.clone(),
                norwegian: t.lang == Lang::Norwegian,
                total_seconds: t.total.as_secs(),
                ends: match t.state {
                    State::Running { ends } => {
                        let left = SignedDuration::try_from(ends.saturating_duration_since(now)).unwrap_or_default();
                        Some(wall.checked_add(left).unwrap_or(wall))
                    }
                    State::Paused { .. } => None,
                },
                left_seconds: match t.state {
                    State::Running { .. } => None,
                    State::Paused { left } => Some(left.as_secs()),
                },
                is_reminder: t.is_reminder,
                is_alarm: t.is_alarm,
                at: t.at.clone(),
                late: t.late.clone(),
            })
            .collect();
        // Written aside and renamed, so a crash never leaves half a file.
        let partial = path.with_extension("part");
        let result = serde_json::to_vec_pretty(&kept)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&partial, bytes))
            .and_then(|()| std::fs::rename(&partial, path));
        if let Err(error) = result {
            warn!(%error, path = %path.display(), "could not keep the timers");
        }
    }

    pub fn list(&self) -> &[Timer] {
        &self.list
    }

    /// When the next timer ends.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.list
            .iter()
            .filter_map(|t| match t.state {
                State::Running { ends } => Some(ends),
                State::Paused { .. } => None,
            })
            .min()
    }

    /// Removes and returns the timers that have ended, with their notices. One that ended while the
    /// engine was down is told, not rung: its notice is a cancel, which clears it from a display.
    /// An alarm rings even then.
    pub fn due(&mut self, now: Instant) -> Vec<(Timer, Vec<Notice>)> {
        let (done, running): (Vec<Timer>, Vec<Timer>) = std::mem::take(&mut self.list)
            .into_iter()
            .partition(|t| matches!(t.state, State::Running { ends } if ends <= now));
        self.list = running;
        if done.is_empty() {
            return Vec::new();
        }
        let done: Vec<(Timer, Vec<Notice>)> = done
            .into_iter()
            .map(|t| {
                let notices = match (t.late.is_some(), t.is_alarm) {
                    (true, false) => vec![Notice::of(&t, NoticeKind::Cancelled, now)],
                    // The device did not hear of it before the restart.
                    (true, true) => {
                        vec![Notice::of(&t, NoticeKind::Started, now), Notice::of(&t, NoticeKind::Finished, now)]
                    }
                    (false, _) => vec![Notice::of(&t, NoticeKind::Finished, now)],
                };
                if t.is_alarm {
                    self.rang = Some((t.clone(), now));
                }
                (t, notices)
            })
            .collect();
        self.save(now);
        done
    }

    /// A satellite connected: the timers it should show, and those it may still show from before a
    /// restart that are gone. Timers that ended while the engine was down are told now.
    pub fn connected(&mut self, now: Instant) -> Vec<Notice> {
        let mut notices = std::mem::take(&mut self.dropped);
        for timer in &mut self.list {
            if timer.late.is_some() {
                timer.state = State::Running { ends: now };
            } else {
                notices.push(Notice::of(timer, NoticeKind::Started, now));
            }
        }
        notices
    }

    /// An alarm went off within the time a device rings for.
    pub fn alarm_ringing(&self, now: Instant) -> bool {
        self.rang.as_ref().is_some_and(|(_, at)| now.saturating_duration_since(*at) < RINGS_FOR)
    }

    /// True when the last answer asked "For how long?" or "When?".
    pub fn awaiting_duration(&self) -> bool {
        self.pending.is_some() || self.choosing.is_some()
    }

    /// True while a "Which one?" waits for its answer.
    pub fn choosing(&self) -> bool {
        self.choosing.is_some()
    }

    /// Drops a "Which one?" whose window has passed, so it no longer waits for an answer.
    pub fn forget_stale(&mut self, now: Instant) {
        if self.choosing.as_ref().is_some_and(|c| now.saturating_duration_since(c.at) > CHOICE_WINDOW) {
            self.choosing = None;
        }
    }

    /// True when `text` is for the question asked: anything after "For how long?", and only an
    /// answer ("the first one", "the pasta one") after "Which one?".
    pub fn takes_answer(&self, text: &str) -> bool {
        self.pending.is_some() || self.choosing.is_some() && answer_of(text).is_some()
    }

    /// Forgets a question that was not answered.
    pub fn clear_pending(&mut self) {
        self.pending = None;
        self.choosing = None;
    }

    /// The answer to "For how long?" or "When should I remind you?": starts the pending timer when
    /// `text` names a duration, or for a reminder or an alarm a time of day.
    pub fn answer_pending(&mut self, text: &str, lang: Lang, now: Instant) -> Option<(String, Vec<Notice>)> {
        if self.choosing.is_some() {
            return self.answer_choice(text, lang, now);
        }
        let pending = self.pending.as_ref()?;
        let when = if pending.is_reminder || pending.is_alarm { when(text)? } else { When::In(duration(text)?) };
        let plan = self.pending.take()?;
        let started = self.start(plan, when, lang, now);
        self.save(now);
        Some(started)
    }

    pub fn run(&mut self, command: TimerCommand, lang: Lang, now: Instant) -> (String, Vec<Notice>) {
        self.pending = None;
        self.choosing = None;
        let no = lang == Lang::Norwegian;
        let answer = match command {
            TimerCommand::Start { seconds: None, name } => {
                self.pending = Some(Plan { name, ..Plan::default() });
                (if no { "Hvor lenge?" } else { "For how long?" }.into(), vec![])
            }
            TimerCommand::Start { seconds: Some(seconds), name } => {
                self.start(Plan { name, ..Plan::default() }, When::In(seconds), lang, now)
            }
            TimerCommand::Remind { when: None, message } => {
                self.pending = Some(Plan { reminder: message, is_reminder: true, ..Plan::default() });
                (if no { "Når skal jeg minne deg på det?" } else { "When should I remind you?" }.into(), vec![])
            }
            TimerCommand::Remind { when: Some(when), message } => {
                self.start(Plan { reminder: message, is_reminder: true, ..Plan::default() }, when, lang, now)
            }
            TimerCommand::Alarm { when: None } => {
                self.pending = Some(Plan { is_alarm: true, ..Plan::default() });
                (if no { "Når skal jeg vekke deg?" } else { "For what time?" }.into(), vec![])
            }
            TimerCommand::Alarm { when: Some(when) } => {
                self.start(Plan { is_alarm: true, ..Plan::default() }, when, lang, now)
            }
            TimerCommand::MoveAlarm(to) => self.move_alarm(to, lang, now),
            TimerCommand::Snooze => self.snooze(SNOOZE, lang, now),
            TimerCommand::StopAlarm => self.stop_alarm(lang, now),
            TimerCommand::Remaining(which) if which.alarm => (self.alarms(lang, now), vec![]),
            TimerCommand::Remaining(which) => (self.remaining(&which, lang, now), vec![]),
            TimerCommand::Cancel(which) => self.change(&which, lang, now, Change::Cancel),
            TimerCommand::Pause(which) => self.change(&which, lang, now, Change::Pause),
            TimerCommand::Resume(which) => self.change(&which, lang, now, Change::Resume),
            TimerCommand::Add { seconds, which } => self.change(&which, lang, now, Change::Add(seconds)),
        };
        self.save(now);
        answer
    }

    fn start(&mut self, plan: Plan, when: When, lang: Lang, now: Instant) -> (String, Vec<Notice>) {
        let no = lang == Lang::Norwegian;
        let local = self.local(now);
        let (seconds, target) = match when {
            When::In(seconds) => (seconds, None),
            When::At(clock) => match clock.next(&local, plan.is_alarm) {
                Some(at) => ((at.timestamp().as_second() - local.timestamp().as_second()).max(1) as u32, Some(at)),
                None => {
                    let text =
                        if no { "Beklager, jeg forstod ikke klokkeslettet." } else { "Sorry, I didn't get that time." };
                    return (text.into(), vec![]);
                }
            },
        };
        self.next_id += 1;
        let total = Duration::from_secs(seconds as u64);
        let ends = target.clone().unwrap_or_else(|| self.local(now + total));
        let timer = Timer {
            id: self.next_id,
            name: plan.name,
            reminder: plan.reminder,
            lang,
            total,
            state: State::Running { ends: now + total },
            is_reminder: plan.is_reminder,
            is_alarm: plan.is_alarm,
            at: plan.is_alarm.then(|| ends.strftime("%H:%M").to_string()),
            late: None,
        };
        let length = say_duration(seconds, lang);
        // The fixed words in a sentence of their own, rendered ahead; the time in the next.
        let text = match (lang, &timer.name, timer.is_reminder, timer.is_alarm, &target) {
            (Lang::English, .., true, None) => format!("Okay, your alarm is set. It goes off in {length}."),
            (Lang::English, .., true, Some(_)) => {
                format!("Okay, your alarm is set. It goes off {}.", say_at(&ends, &local, lang))
            }
            (Lang::Norwegian, .., true, None) => format!("Ok, vekkerklokka er stilt. Den ringer om {length}."),
            (Lang::Norwegian, .., true, Some(_)) => {
                format!("Ok, vekkerklokka er stilt. Den ringer {}.", say_at(&ends, &local, lang))
            }
            (Lang::English, _, true, _, None) => format!("Okay, I'll remind you in {length}."),
            (Lang::English, _, true, _, Some(_)) => {
                format!("Okay, I'll remind you. That's {}.", say_at(&ends, &local, lang))
            }
            (Lang::Norwegian, _, true, _, None) => format!("Ok, jeg minner deg på det om {length}."),
            (Lang::Norwegian, _, true, _, Some(_)) => {
                format!("Ok, jeg minner deg på det. Det blir {}.", say_at(&ends, &local, lang))
            }
            (Lang::English, Some(name), ..) => format!("{} timer set for {length}.", capitalize(name)),
            (Lang::English, None, ..) => format!("Timer set for {length}."),
            (Lang::Norwegian, Some(name), ..) => format!("{}-timer satt på {length}.", capitalize(name)),
            (Lang::Norwegian, None, ..) => format!("Timer satt på {length}."),
        };
        let notice = Notice::of(&timer, NoticeKind::Started, now);
        self.list.push(timer);
        (text, vec![notice])
    }

    /// The alarm that is set, to a new time: it is replaced, so the device shows the new length.
    fn move_alarm(&mut self, to: Move, lang: Lang, now: Instant) -> (String, Vec<Notice>) {
        let no = lang == Lang::Norwegian;
        // The ringing one is no longer in the list: "add 10 minutes to my alarm" puts it off.
        if let Move::By(by) = to {
            if by > 0 && self.alarm_ringing(now) {
                return self.snooze(by.min(i64::from(u32::MAX)) as u32, lang, now);
            }
        }
        let alarms: Vec<usize> = (0..self.list.len()).filter(|&i| self.list[i].is_alarm).collect();
        let index = match (alarms.as_slice(), to) {
            ([], Move::To(when)) => return self.start(Plan { is_alarm: true, ..Plan::default() }, when, lang, now),
            ([], Move::By(_)) => return (no_alarm(no), vec![]),
            ([one], _) => *one,
            (several, _) => {
                // Which one: the same question a cancel asks, kept for the answer.
                self.choosing = Some(Choosing::new(Choice::Move(to), several, &self.list, now));
                return self.change(&Which { alarm: true, ..Which::default() }, lang, now, Change::Ask);
            }
        };
        self.move_alarm_at(index, to, lang, now)
    }

    fn move_alarm_at(&mut self, index: usize, to: Move, lang: Lang, now: Instant) -> (String, Vec<Notice>) {
        let no = lang == Lang::Norwegian;
        let when = match to {
            Move::To(when) => when,
            Move::By(by) => {
                let left = self.list[index].left(now).as_secs() as i64 + by;
                if left <= 0 {
                    let text = if no {
                        "Beklager, det tidspunktet har allerede vært."
                    } else {
                        "Sorry, that time has already passed."
                    };
                    return (text.into(), vec![]);
                }
                When::In(left.min(i64::from(u32::MAX)) as u32)
            }
        };
        let old = self.list.remove(index);
        let count = self.list.len();
        let (failed, mut notices) =
            self.start(Plan { name: old.name.clone(), is_alarm: true, ..Plan::default() }, when, lang, now);
        if self.list.len() == count {
            // A time that could not be read: the old alarm stays.
            self.list.insert(index, old);
            return (failed, notices);
        }
        notices.insert(0, Notice::of(&old, NoticeKind::Cancelled, now));
        let ends = self.local(now + self.list[count].left(now));
        let at = say_at(&ends, &self.local(now), lang);
        let text = if no {
            format!("Ok, vekkerklokka er flyttet. Den ringer {at}.")
        } else {
            format!("Okay, your alarm is moved. It goes off {at}.")
        };
        (text, notices)
    }

    fn snooze(&mut self, seconds: u32, lang: Lang, now: Instant) -> (String, Vec<Notice>) {
        let no = lang == Lang::Norwegian;
        if !self.alarm_ringing(now) {
            return (if no { "Ingen vekkerklokke har ringt." } else { "There's no alarm to snooze." }.into(), vec![]);
        }
        let Some((alarm, _)) = self.rang.take() else { return (String::new(), vec![]) };
        let plan = Plan { name: alarm.name, is_alarm: true, ..Plan::default() };
        let (_, notices) = self.start(plan, When::In(seconds), lang, now);
        let length = say_duration(seconds, lang);
        let text =
            if no { format!("Ok, jeg vekker deg igjen om {length}.") } else { format!("Okay, snoozing for {length}.") };
        (text, notices)
    }

    fn stop_alarm(&mut self, lang: Lang, now: Instant) -> (String, Vec<Notice>) {
        let no = lang == Lang::Norwegian;
        if self.alarm_ringing(now) {
            self.rang = None;
            return (
                if no { "Ok, jeg har stoppet vekkerklokka." } else { "Okay, I've stopped the alarm." }.into(),
                vec![],
            );
        }
        if self.list.iter().any(|t| t.is_alarm) {
            return self.change(&Which { alarm: true, ..Which::default() }, lang, now, Change::Cancel);
        }
        (if no { "Ingen vekkerklokke ringer." } else { "There's no alarm ringing." }.into(), vec![])
    }

    /// "Your alarm goes off at 07:00."
    fn alarms(&self, lang: Lang, now: Instant) -> String {
        let no = lang == Lang::Norwegian;
        let local = self.local(now);
        let mut alarms: Vec<&Timer> = self.list.iter().filter(|t| t.is_alarm).collect();
        alarms.sort_by_key(|t| t.left(now));
        let times: Vec<String> = alarms.iter().map(|t| say_at(&self.local(now + t.left(now)), &local, lang)).collect();
        match (times.as_slice(), no) {
            ([], _) => no_alarm(no),
            ([one], false) => format!("Your alarm goes off {one}."),
            ([one], true) => format!("Vekkerklokka ringer {one}."),
            (_, false) => format!("Your alarms go off {}.", join(&times, no)),
            (_, true) => format!("Vekkerklokkene ringer {}.", join(&times, no)),
        }
    }

    /// The timers of the kind a request is about: alarms, or the others.
    fn of_kind(&self, which: &Which) -> Vec<usize> {
        (0..self.list.len()).filter(|&i| self.list[i].is_alarm == which.alarm).collect()
    }

    /// The timers a request means. A name that matches nothing still means the only timer there
    /// is: names get misheard, and the language model invents them ("the pizza").
    fn select(&self, which: &Which) -> Vec<usize> {
        let found: Vec<usize> = (0..self.list.len()).filter(|&i| self.list[i].matches(which)).collect();
        let kind = self.of_kind(which);
        if found.is_empty() && kind.len() == 1 && which.seconds.is_none() && !which.reminder {
            return kind;
        }
        found
    }

    fn remaining(&self, which: &Which, lang: Lang, now: Instant) -> String {
        let no = lang == Lang::Norwegian;
        let mut found: Vec<&Timer> = self.select(which).into_iter().map(|i| &self.list[i]).collect();
        found.sort_by_key(|t| t.left(now));
        let only = self.of_kind(which).len() == 1;
        let describe = |t: &Timer, single: bool| {
            let left = say_left(t.left(now), lang);
            let label = t.label(lang);
            match (no, t.is_running(), single) {
                // The only timer needs no name, and the time left alone is a sentence rendered ahead.
                (false, true, true) if only => format!("{} left", capitalize(&left)),
                (false, true, true) => format!("{} left on {label}", capitalize(&left)),
                (false, true, false) => format!("{label} has {left} left"),
                (false, false, _) => format!("{label} is paused, with {left} left"),
                (true, true, true) => format!("{} igjen på {label}", capitalize(&left)),
                (true, true, false) => format!("{label} har {left} igjen"),
                (true, false, _) => format!("{label} står på pause, med {left} igjen"),
            }
        };
        match found.as_slice() {
            [] if self.of_kind(which).is_empty() => none_running(which, no),
            [] => not_found(which, no),
            [one] => capitalize(&format!("{}.", describe(one, true))),
            several => {
                let parts: Vec<String> = several.iter().map(|t| describe(t, false)).collect();
                capitalize(&format!("{}.", join(&parts, no)))
            }
        }
    }

    fn change(&mut self, which: &Which, lang: Lang, now: Instant, change: Change) -> (String, Vec<Notice>) {
        let no = lang == Lang::Norwegian;
        let found = self.select(which);
        let (noun, noun_no) = if which.alarm { ("alarms", "vekkerklokker") } else { ("timers", "timere") };
        let targets = match found.as_slice() {
            [] if self.of_kind(which).is_empty() => return (none_running(which, no), vec![]),
            [] => return (not_found(which, no), vec![]),
            [_] => found,
            several if which.all => several.to_vec(),
            several => {
                if change != Change::Ask {
                    self.choosing = Some(Choosing::new(Choice::Change(change), several, &self.list, now));
                }
                let labels: Vec<String> = several.iter().map(|&i| self.list[i].label(lang)).collect();
                let text = if no {
                    format!("Du har {} {noun_no}: {}. Hvilken?", several.len(), join(&labels, no))
                } else {
                    format!("You have {} {noun}: {}. Which one?", several.len(), join(&labels, no))
                };
                return (text, vec![]);
            }
        };
        self.apply(targets, which.alarm, lang, now, change)
    }

    /// The answer to "Which one?": "the first one", "the 7 o'clock one", "the pasta one", "both".
    /// `None` when it is not shaped like an answer, comes too late or names none of them, so the
    /// words are taken as a new request.
    fn answer_choice(&mut self, text: &str, lang: Lang, now: Instant) -> Option<(String, Vec<Notice>)> {
        let no = lang == Lang::Norwegian;
        let asked = self.choosing.take()?;
        if now.saturating_duration_since(asked.at) > CHOICE_WINDOW {
            return None;
        }
        let answer = answer_of(text)?;
        // In the order they were said; one that has ended since is gone, but keeps its place.
        let timers: Vec<Option<&Timer>> = asked.ids.iter().map(|id| self.list.iter().find(|t| t.id == *id)).collect();
        let picked = chosen(&answer, &timers, now)?;
        let found: Vec<usize> = picked
            .iter()
            .filter_map(|&k| timers[k])
            .filter_map(|t| self.list.iter().position(|l| l.id == t.id))
            .collect();
        if found.is_empty() {
            let text = if no { "Den er allerede ferdig." } else { "That one has already ended." };
            return Some((text.into(), vec![]));
        }
        let answer = match asked.choice {
            Choice::Move(to) => match found.as_slice() {
                [one] => self.move_alarm_at(*one, to, lang, now),
                _ => {
                    let text = if no { "Jeg kan bare flytte én om gangen." } else { "I can only move one at a time." };
                    return Some((text.into(), vec![]));
                }
            },
            Choice::Change(change) => {
                let alarm = self.list[found[0]].is_alarm;
                self.apply(found, alarm, lang, now, change)
            }
        };
        self.save(now);
        Some(answer)
    }

    fn apply(
        &mut self,
        mut targets: Vec<usize>,
        alarm: bool,
        lang: Lang,
        now: Instant,
        change: Change,
    ) -> (String, Vec<Notice>) {
        let no = lang == Lang::Norwegian;
        let noun = if alarm { "alarms" } else { "timers" };
        // Removed from the end first, so the indices before stay right.
        targets.sort_unstable();
        let mut notices = Vec::new();
        let count = targets.len();
        let label = self.list[targets[0]].label(lang);
        for &i in targets.iter().rev() {
            let timer = &mut self.list[i];
            let kind = match change {
                Change::Ask => continue,
                Change::Cancel => NoticeKind::Cancelled,
                Change::Pause => {
                    timer.state = State::Paused { left: timer.left(now) };
                    NoticeKind::Updated
                }
                Change::Resume => {
                    timer.state = State::Running { ends: now + timer.left(now) };
                    NoticeKind::Updated
                }
                Change::Add(seconds) => {
                    let extra = Duration::from_secs(seconds as u64);
                    timer.total += extra;
                    timer.state = match timer.state {
                        State::Running { ends } => State::Running { ends: ends + extra },
                        State::Paused { left } => State::Paused { left: left + extra },
                    };
                    NoticeKind::Updated
                }
            };
            notices.push(Notice::of(timer, kind, now));
            if change == Change::Cancel {
                self.list.remove(i);
            }
        }
        let label = capitalize(&label);
        let all_no = if alarm { format!("Alle {count} vekkerklokkene") } else { format!("Alle {count} timerne") };
        let text = match (no, change) {
            (_, Change::Ask) => String::new(),
            (false, Change::Cancel) if count > 1 => format!("All {count} {noun} are cancelled."),
            (true, Change::Cancel) if count > 1 => format!("{all_no} er avbrutt."),
            (false, Change::Pause) if count > 1 => format!("All {count} {noun} are paused."),
            (true, Change::Pause) if count > 1 => format!("{all_no} er satt på pause."),
            (false, Change::Resume) if count > 1 => format!("All {count} {noun} are running again."),
            (true, Change::Resume) if count > 1 => format!("{all_no} går igjen."),
            (false, Change::Cancel) => format!("{label} is cancelled."),
            (true, Change::Cancel) => format!("{label} er avbrutt."),
            (false, Change::Pause) => format!("{label} is paused."),
            (true, Change::Pause) => format!("{label} er satt på pause."),
            (false, Change::Resume) => format!("{label} is running again."),
            (true, Change::Resume) => format!("{label} går igjen."),
            (false, Change::Add(seconds)) => {
                format!("Added {}. {label} ends in {}.", say_duration(seconds, lang), left_of(&notices, lang))
            }
            (true, Change::Add(seconds)) => {
                format!("La til {}. {label} er ferdig om {}.", say_duration(seconds, lang), left_of(&notices, lang))
            }
        };
        (text, notices)
    }
}

/// What a "Which one?" was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Move(Move),
    Change(Change),
}

/// How long a "Which one?" waits for its answer, as long as a question waiting for "yes".
const CHOICE_WINDOW: Duration = Duration::from_secs(30);

/// A "Which one?" asked: what for, the timers it listed in the order said, and when.
struct Choosing {
    choice: Choice,
    ids: Vec<u32>,
    at: Instant,
}

impl Choosing {
    fn new(choice: Choice, listed: &[usize], list: &[Timer], at: Instant) -> Self {
        Self { choice, ids: listed.iter().map(|&i| list[i].id).collect(), at }
    }
}

/// An answer to "Which one?", as said.
#[derive(Debug, Clone, PartialEq)]
enum Answer {
    /// "The first one": a place in the list as said, from 0.
    Nth(usize),
    Last,
    /// "Both", "all of them", "begge".
    All,
    /// "The pasta one", "pastatimeren": the words of a name.
    Named(Vec<String>),
    /// "The 7 o'clock one", "the 7 pm one".
    At(Clock),
    /// "The 10 minute one".
    Lasting(u32),
}

/// Words that only frame an answer: "the ... one", "den ...", "of them".
const ANSWER_FILLER: &[&str] = &[
    "the",
    "one",
    "ones",
    "of",
    "them",
    "please",
    "den",
    "det",
    "de",
    "dem",
    "sammen",
    "takk",
    "timer",
    "timeren",
    "alarm",
    "vekkerklokka",
    "reminder",
    "påminnelsen",
    "på",
];

const ORDINALS: &[&[&str]] = &[
    &["first", "1st", "første", "forste"],
    &["second", "2nd", "andre"],
    &["third", "3rd", "tredje"],
    &["fourth", "4th", "fjerde"],
];

/// `text` as an answer to "Which one?", if it is nothing but one. Every word must be part of the
/// answer or frame it, so a reply with a verb of its own ("pause the pasta one", "volume 7",
/// "wait 5 minutes") is a new request, and "one" is never one o'clock.
fn answer_of(text: &str) -> Option<Answer> {
    let words = tokens(text);
    let content: Vec<String> = words.iter().filter(|w| !ANSWER_FILLER.contains(&w.as_str())).cloned().collect();
    // A verb makes it a request of its own: "cancel the pasta timer" runs as said.
    const VERBS: &[&str] = &[
        "cancel", "stop", "pause", "resume", "delete", "remove", "move", "change", "add", "set", "turn", "switch",
        "play", "wait", "volume", "lights", "light", "avbryt", "stopp", "slett", "fjern", "flytt", "sett", "skru",
        "slå", "spill", "vent", "lyset", "lys", "glem", "forget",
    ];
    if content.is_empty() || words.len() > 6 || content.iter().any(|w| VERBS.contains(&w.as_str())) {
        return None;
    }
    let only = |list: &[&str]| content.iter().all(|w| list.contains(&w.as_str()));
    if only(&["both", "all", "begge", "alle", "to", "two"]) && !only(&["to", "two"]) {
        return Some(Answer::All);
    }
    if let [word] = content.as_slice() {
        if ["last", "siste"].contains(&word.as_str()) {
            return Some(Answer::Last);
        }
        if let Some(k) = ORDINALS.iter().position(|o| o.contains(&word.as_str())) {
            return Some(Answer::Nth(k));
        }
    }
    // A time, with nothing else but "o'clock", am or pm, "in the evening".
    if let Some((clock, used)) = clock_words(&content, &[], true) {
        let rest = |k: usize| used[k] || ["o'clock", "oclock", "klokka", "kl"].contains(&content[k].as_str());
        if (0..content.len()).all(rest) {
            return Some(Answer::At(clock));
        }
    }
    let (seconds, used) = duration_words(&content);
    if let Some(seconds) = seconds.filter(|_| used.iter().all(|&u| u)) {
        return Some(Answer::Lasting(seconds));
    }
    // A name: no digits, and few words ("the pasta one", "pasta-timeren", "pastatimeren").
    let name: Vec<String> = content.iter().map(|w| compound_name(w).unwrap_or(w).to_owned()).collect();
    (name.len() <= 2 && name.iter().all(|w| w.chars().all(char::is_alphabetic) && w.chars().count() > 2))
        .then_some(Answer::Named(name))
}

/// Which of `timers` (in the order they were listed; `None` for one that has ended) the answer
/// names.
fn chosen(answer: &Answer, timers: &[Option<&Timer>], now: Instant) -> Option<Vec<usize>> {
    if timers.is_empty() {
        return None;
    }
    let live = || (0..timers.len()).filter_map(|k| timers[k].map(|t| (k, t)));
    let one = |found: Vec<usize>| (found.len() == 1).then_some(found);
    match answer {
        Answer::All => Some((0..timers.len()).collect()),
        Answer::Last => Some(vec![timers.len() - 1]),
        Answer::Nth(k) => (*k < timers.len()).then(|| vec![*k]),
        // Against the 24-hour time: "the 7 pm one" is 19:00; a bare 7 is whichever comes next.
        Answer::At(clock) => {
            let hours: Vec<u8> = match clock.pm {
                _ if clock.hour == 0 || clock.hour > 12 => vec![clock.hour],
                Some(true) => vec![clock.hour % 12 + 12],
                Some(false) => vec![clock.hour % 12],
                None => vec![clock.hour % 12, clock.hour % 12 + 12],
            };
            live()
                .filter(|(_, t)| {
                    t.at.as_deref().and_then(|at| at.split_once(':')).is_some_and(|(h, m)| {
                        h.parse::<u8>().is_ok_and(|h| hours.contains(&h)) && m.parse::<u8>() == Ok(clock.minute)
                    })
                })
                .min_by_key(|(_, t)| t.left(now))
                .map(|(k, _)| vec![k])
        }
        Answer::Lasting(seconds) => {
            one(live().filter(|(_, t)| t.total.as_secs() as u32 == *seconds).map(|(k, _)| k).collect())
        }
        // By the words of its name or message, every word said.
        Answer::Named(name) => one(live()
            .filter(|(_, t)| {
                let own: Vec<String> =
                    [t.name.as_deref(), t.reminder.as_deref()].iter().flatten().flat_map(|h| tokens(h)).collect();
                name.iter().all(|w| own.contains(w))
            })
            .map(|(k, _)| k)
            .collect()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// Nothing changes: for "which one?" when several match.
    Ask,
    Cancel,
    Pause,
    Resume,
    Add(u32),
}

fn left_of(notices: &[Notice], lang: Lang) -> String {
    say_left(Duration::from_secs(notices.first().map_or(0, |n| n.seconds_left) as u64), lang)
}

fn none_running(which: &Which, no: bool) -> String {
    if which.alarm {
        return no_alarm(no);
    }
    if no { "Ingen timere er i gang." } else { "There are no timers running." }.into()
}

fn no_alarm(no: bool) -> String {
    if no { "Du har ingen vekkerklokke stilt." } else { "You have no alarm set." }.into()
}

fn not_found(which: &Which, no: bool) -> String {
    match (&which.name, no) {
        (Some(name), false) => format!("I couldn't find a {name} timer."),
        (Some(name), true) => format!("Jeg fant ingen timer som heter {name}."),
        (None, false) => "I couldn't find that timer.".into(),
        (None, true) => "Jeg fant ikke den timeren.".into(),
    }
}

/// "at 17:30", "tomorrow at 08:00", "on Monday at 08:00"; Norwegian "klokka 17:30", "i morgen
/// klokka 08:00", "på mandag klokka 08:00". Anything later than tomorrow gets its weekday, so a
/// time a day off is never called "tomorrow".
fn say_at(at: &Zoned, now: &Zoned, lang: Lang) -> String {
    let time = at.strftime("%H:%M");
    let day = if at.date() == now.date() {
        None
    } else if now.date().tomorrow().is_ok_and(|t| t == at.date()) {
        Some(if lang == Lang::Norwegian { "i morgen".to_owned() } else { "tomorrow".to_owned() })
    } else {
        Some(weekday(at, lang))
    };
    match (lang, day) {
        (Lang::English, None) => format!("at {time}"),
        (Lang::English, Some(day)) => format!("{day} at {time}"),
        (Lang::Norwegian, None) => format!("klokka {time}"),
        (Lang::Norwegian, Some(day)) => format!("{day} klokka {time}"),
    }
}

/// "on Monday", "på mandag".
fn weekday(at: &Zoned, lang: Lang) -> String {
    use jiff::civil::Weekday::*;
    let (en, no) = match at.weekday() {
        Monday => ("Monday", "mandag"),
        Tuesday => ("Tuesday", "tirsdag"),
        Wednesday => ("Wednesday", "onsdag"),
        Thursday => ("Thursday", "torsdag"),
        Friday => ("Friday", "fredag"),
        Saturday => ("Saturday", "lørdag"),
        Sunday => ("Sunday", "søndag"),
    };
    if lang == Lang::Norwegian {
        format!("på {no}")
    } else {
        format!("on {en}")
    }
}

/// Fixed answers about alarms and reminders, to render ahead.
pub const ENGLISH_ANSWERS: &[&str] = &[
    "Okay, your alarm is set.",
    "Okay, I'll remind you.",
    "Okay, your alarm is moved.",
    "Okay, I've stopped the alarm.",
    "There's no alarm ringing.",
    "There's no alarm to snooze.",
    "You have no alarm set.",
    "For what time?",
    "When should I remind you?",
    "Time to wake up.",
];

/// What to say when a timer ends.
pub fn finished_text(timer: &Timer) -> String {
    let no = timer.lang == Lang::Norwegian;
    if timer.is_alarm {
        return if no { "På tide å stå opp." } else { "Time to wake up." }.into();
    }
    let on_time = match (timer.lang, &timer.reminder, timer.is_reminder) {
        (Lang::English, Some(message), _) => format!("Reminder: {message}."),
        (Lang::English, None, true) => "This is your reminder.".into(),
        (Lang::English, None, false) => {
            format!("{} is done.", capitalize(&timer.label(Lang::English).replacen("the ", "your ", 1)))
        }
        (Lang::Norwegian, Some(message), _) => format!("Påminnelse: {message}."),
        (Lang::Norwegian, None, true) => "Her er påminnelsen din.".into(),
        (Lang::Norwegian, None, false) => {
            format!("{} er ferdig.", capitalize(&timer.label(Lang::Norwegian)))
        }
    };
    match (&timer.late, timer.is_reminder, no) {
        (None, ..) => on_time,
        (Some(due), true, false) => format!("{on_time} It was due at {due}."),
        (Some(due), true, true) => format!("{on_time} Den skulle kommet klokka {due}."),
        (Some(_), false, false) => {
            format!(
                "{} ended while I was restarting.",
                capitalize(&timer.label(Lang::English).replacen("the ", "your ", 1))
            )
        }
        (Some(_), false, true) => {
            format!("{} ble ferdig mens jeg startet på nytt.", capitalize(&timer.label(Lang::Norwegian)))
        }
    }
}

/// "10 minutes", "1 hour and 30 minutes", "1 time og 30 minutter".
pub fn say_duration(seconds: u32, lang: Lang) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    let names: [(&str, &str); 3] = match lang {
        Lang::English => [("hour", "hours"), ("minute", "minutes"), ("second", "seconds")],
        Lang::Norwegian => [("time", "timer"), ("minutt", "minutter"), ("sekund", "sekunder")],
    };
    let parts: Vec<String> = [h, m, s]
        .iter()
        .zip(names)
        .filter(|(n, _)| **n > 0)
        .map(|(&n, (one, many))| format!("{n} {}", if n == 1 { one } else { many }))
        .collect();
    if parts.is_empty() {
        return match lang {
            Lang::English => "0 seconds".into(),
            Lang::Norwegian => "0 sekunder".into(),
        };
    }
    join(&parts, lang == Lang::Norwegian)
}

/// "6 minutes and 40 seconds left.": the time left on the only timer, every way it is said up
/// to two hours, to render ahead.
pub fn english_left_sentences() -> Vec<String> {
    (1..600)
        .chain((600..=7200).step_by(60))
        .map(|seconds| format!("{} left.", capitalize(&say_left(Duration::from_secs(seconds), Lang::English))))
        .collect()
}

/// Time left, rounded the way people say it: seconds only under ten minutes.
fn say_left(left: Duration, lang: Lang) -> String {
    let seconds = left.as_secs_f32().ceil() as u32;
    let rounded = if seconds >= 600 { (seconds + 30) / 60 * 60 } else { seconds };
    if seconds == 0 {
        return match lang {
            Lang::English => "less than a second".into(),
            Lang::Norwegian => "under ett sekund".into(),
        };
    }
    say_duration(rounded, lang)
}

/// "10 minute" for a label, when the length is one unit only.
fn single_unit(seconds: u32) -> Option<(u32, &'static str)> {
    match seconds {
        0 => None,
        s if s % 3600 == 0 => Some((s / 3600, "hour")),
        s if s % 60 == 0 && s < 3600 => Some((s / 60, "minute")),
        s if s < 60 => Some((s, "second")),
        _ => None,
    }
}

/// "a, b and c" / "a, b og c".
fn join(parts: &[String], no: bool) -> String {
    let and = if no { " og " } else { " and " };
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{}{and}{last}", rest.join(", ")),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TimerCommand::*;
    use When::{At, In};

    fn start(seconds: u32) -> Option<TimerCommand> {
        Some(Start { seconds: Some(seconds), name: None })
    }

    #[test]
    fn durations_in_words_and_digits() {
        for (text, seconds) in [
            ("10 minutes", 600),
            ("ten minutes", 600),
            ("twenty five minutes", 1500),
            ("twenty-five minutes", 1500),
            ("a minute", 60),
            ("an hour", 3600),
            ("half an hour", 1800),
            ("an hour and a half", 5400),
            ("one and a half hours", 5400),
            ("1.5 hours", 5400),
            ("1 hour and 30 minutes", 5400),
            ("2 minutes 30 seconds", 150),
            ("90 seconds", 90),
            ("a quarter of an hour", 900),
            ("ti minutter", 600),
            ("tjuefem minutter", 1500),
            ("tjue fem minutter", 1500),
            ("en time", 3600),
            ("to timer", 7200),
            ("en halv time", 1800),
            ("en halvtime", 1800),
            ("halvannen time", 5400),
            ("en og en halv time", 5400),
            ("1,5 time", 5400),
            ("et kvarter", 900),
            ("tre kvarter", 2700),
            ("ett minutt", 60),
            ("30 sekunder", 30),
        ] {
            assert_eq!(duration(text), Some(seconds), "{text}");
        }
        assert_eq!(duration("a timer"), None);
        assert_eq!(duration("en timer"), None);
        assert_eq!(duration("set the timer to go"), None);
    }

    #[test]
    fn starting_timers() {
        assert_eq!(parse("Set a timer for 10 minutes."), start(600));
        assert_eq!(parse("set a 10-minute timer"), start(600));
        assert_eq!(parse("timer for five minutes please"), start(300));
        assert_eq!(parse("start a timer for an hour and a half"), start(5400));
        assert_eq!(parse("Sett en timer på ti minutter."), start(600));
        assert_eq!(parse("sett en timer på to timer"), start(7200));
        assert_eq!(parse("start en nedtelling på 3 minutter"), start(180));
        assert_eq!(
            parse("set a pasta timer for 8 minutes"),
            Some(Start { seconds: Some(480), name: Some("pasta".into()) })
        );
        assert_eq!(
            parse("set a timer called pizza for 12 minutes"),
            Some(Start { seconds: Some(720), name: Some("pizza".into()) })
        );
        assert_eq!(
            parse("sett en pastatimer på 8 minutter"),
            Some(Start { seconds: Some(480), name: Some("pasta".into()) })
        );
        assert_eq!(parse("set a timer"), Some(Start { seconds: None, name: None }));
    }

    #[test]
    fn reminders() {
        let reminder = |seconds: Option<u32>, message: &str| {
            Some(Remind { when: seconds.map(In), message: Some(message.to_owned()) })
        };
        assert_eq!(parse("Remind me in 10 minutes to check the oven."), reminder(Some(600), "check the oven"));
        assert_eq!(parse("remind me to call my mother in an hour"), reminder(Some(3600), "call your mother"));
        assert_eq!(parse("Minn meg på å ringe mamma om en time."), reminder(Some(3600), "ringe mamma"));
        assert_eq!(parse("remind me to water the plants"), reminder(None, "water the plants"));
        assert_eq!(parse("remind me in five minutes"), Some(Remind { when: Some(In(300)), message: None }));
    }

    #[test]
    fn other_timer_requests() {
        let all = Which { all: true, ..Which::default() };
        assert_eq!(parse("cancel the timer"), Some(Cancel(Which::default())));
        assert_eq!(parse("stop the timer"), Some(Cancel(Which::default())));
        assert_eq!(parse("cancel all timers"), Some(Cancel(all.clone())));
        assert_eq!(parse("avbryt timeren"), Some(Cancel(Which::default())));
        assert_eq!(parse("stopp alle timerne"), Some(Cancel(all)));
        assert_eq!(
            parse("cancel the pasta timer"),
            Some(Cancel(Which { name: Some("pasta".into()), ..Which::default() }))
        );
        assert_eq!(parse("cancel the 10 minute timer"), Some(Cancel(Which { seconds: Some(600), ..Which::default() })));
        assert_eq!(parse("pause the timer"), Some(Pause(Which::default())));
        assert_eq!(parse("resume the timer"), Some(Resume(Which::default())));
        assert_eq!(parse("add 5 minutes to the timer"), Some(Add { seconds: 300, which: Which::default() }));
        assert_eq!(parse("how much time is left?"), Some(Remaining(Which::default())));
        assert_eq!(parse("how long is left on the timer"), Some(Remaining(Which::default())));
        assert_eq!(parse("hvor lang tid er det igjen på timeren?"), Some(Remaining(Which::default())));
        assert_eq!(parse("hvor lang tid er det igjen"), Some(Remaining(Which::default())));
        assert_eq!(parse("what timers do I have"), Some(Remaining(Which::default())));
        assert_eq!(parse("cancel the reminder"), Some(Cancel(Which { reminder: true, ..Which::default() })));
        // Not timers.
        assert_eq!(parse("what time is it"), None);
        assert_eq!(parse("turn off the lights"), None);
    }

    #[test]
    fn a_timer_runs_out() {
        let t0 = Instant::now();
        let mut timers = Timers::default();
        let (text, notices) = timers.run(start(600).unwrap(), Lang::English, t0);
        assert_eq!(text, "Timer set for 10 minutes.");
        assert_eq!(notices[0].kind, NoticeKind::Started);
        assert_eq!(notices[0].seconds_left, 600);
        assert_eq!(timers.next_deadline(), Some(t0 + Duration::from_secs(600)));
        let (text, _) = timers.run(Remaining(Which::default()), Lang::English, t0 + Duration::from_secs(200));
        assert_eq!(text, "6 minutes and 40 seconds left.");
        let left = english_left_sentences();
        assert!(left.contains(&text));
        assert!(left.iter().all(|s| crate::speech_cache::CachedTts::cacheable(s)), "{left:?}");
        assert!(timers.due(t0 + Duration::from_secs(599)).is_empty());
        let done = timers.due(t0 + Duration::from_secs(600));
        assert_eq!(done.len(), 1);
        assert_eq!(finished_text(&done[0].0), "Your 10 minute timer is done.");
        assert_eq!(done[0].1[0].kind, NoticeKind::Finished);
        assert!(timers.list().is_empty() && timers.next_deadline().is_none());
    }

    #[test]
    fn several_timers_need_a_choice() {
        let t0 = Instant::now();
        let mut timers = Timers::default();
        let pasta = Start { seconds: Some(480), name: Some("pasta".into()) };
        assert_eq!(timers.run(pasta, Lang::English, t0).0, "Pasta timer set for 8 minutes.");
        timers.run(start(600).unwrap(), Lang::English, t0);
        let (text, _) = timers.run(Remaining(Which::default()), Lang::English, t0 + Duration::from_secs(60));
        assert_eq!(text, "The pasta timer has 7 minutes left and the 10 minute timer has 9 minutes left.");
        let (text, notices) = timers.run(Cancel(Which::default()), Lang::English, t0);
        assert_eq!(text, "You have 2 timers: the pasta timer and the 10 minute timer. Which one?");
        assert!(notices.is_empty());
        let which = Which { name: Some("pasta".into()), ..Which::default() };
        let (text, notices) = timers.run(Cancel(which.clone()), Lang::English, t0);
        assert_eq!(text, "The pasta timer is cancelled.");
        assert_eq!(notices[0].kind, NoticeKind::Cancelled);
        assert_eq!(timers.list().len(), 1);
        // With one timer left, a name that fits nothing still means it.
        let (text, _) = timers.run(Remaining(which), Lang::English, t0 + Duration::from_secs(60));
        assert_eq!(text, "9 minutes left.");
    }

    #[test]
    fn pause_resume_and_add() {
        let t0 = Instant::now();
        let mut timers = Timers::default();
        timers.run(start(300).unwrap(), Lang::Norwegian, t0);
        let (text, _) = timers.run(Pause(Which::default()), Lang::Norwegian, t0 + Duration::from_secs(100));
        assert_eq!(text, "Timeren på 5 minutter er satt på pause.");
        assert_eq!(timers.next_deadline(), None);
        let (text, _) = timers.run(Resume(Which::default()), Lang::Norwegian, t0 + Duration::from_secs(1000));
        assert_eq!(text, "Timeren på 5 minutter går igjen.");
        assert_eq!(timers.next_deadline(), Some(t0 + Duration::from_secs(1200)));
        let (text, notices) =
            timers.run(Add { seconds: 60, which: Which::default() }, Lang::English, t0 + Duration::from_secs(1000));
        assert_eq!(text, "Added 1 minute. The 5 minute timer ends in 4 minutes and 20 seconds.");
        assert_eq!((notices[0].total_seconds, notices[0].seconds_left), (360, 260));
    }

    #[test]
    fn asking_for_the_duration() {
        let t0 = Instant::now();
        let mut timers = Timers::default();
        assert_eq!(timers.run(Start { seconds: None, name: None }, Lang::English, t0).0, "For how long?");
        assert!(timers.awaiting_duration());
        assert!(timers.answer_pending("banana", Lang::English, t0).is_none());
        let (text, _) = timers.answer_pending("five minutes", Lang::English, t0).unwrap();
        assert_eq!(text, "Timer set for 5 minutes.");
        assert!(!timers.awaiting_duration());
    }

    #[test]
    fn reminders_speak_their_message() {
        let t0 = Instant::now();
        let mut timers = Timers::default();
        let command = parse("remind me in 10 seconds to check the oven").unwrap();
        let (text, notices) = timers.run(command, Lang::English, t0);
        assert_eq!(text, "Okay, I'll remind you in 10 seconds.");
        assert!(notices[0].reminder);
        let done = timers.due(t0 + Duration::from_secs(10));
        assert_eq!(finished_text(&done[0].0), "Reminder: check the oven.");
    }

    fn clock(hour: u8, minute: u8, pm: Option<bool>, tomorrow: bool) -> Option<When> {
        Some(At(Clock { hour, minute, pm, tomorrow }))
    }

    /// 31 October 2026 at `hour`:`minute`, an hour ahead of UTC (no time zone database needed).
    fn local(hour: i8, minute: i8) -> Zoned {
        jiff::civil::date(2026, 10, 31)
            .at(hour, minute, 0, 0)
            .to_zoned(jiff::tz::TimeZone::fixed(jiff::tz::offset(1)))
            .unwrap()
    }

    fn timers_at(t0: Instant, hour: i8, minute: i8) -> Timers {
        Timers::default().with_clock(t0, local(hour, minute))
    }

    #[test]
    fn reminders_at_a_time_of_day() {
        let remind = |when: Option<When>, message: &str| Some(Remind { when, message: Some(message.to_owned()) });
        assert_eq!(parse("Remind me at 5 to call mum."), remind(clock(5, 0, None, false), "call mum"));
        assert_eq!(parse("remind me at 5 pm to call mum"), remind(clock(5, 0, Some(true), false), "call mum"));
        assert_eq!(parse("Remind me at 5 p.m. to call mum."), remind(clock(5, 0, Some(true), false), "call mum"));
        assert_eq!(parse("remind me at 5pm to call mum"), remind(clock(5, 0, Some(true), false), "call mum"));
        assert_eq!(
            parse("remind me at 17:30 to take the bins out"),
            remind(clock(17, 30, None, false), "take the bins out")
        );
        assert_eq!(parse("remind me to call mum at half past six"), remind(clock(6, 30, None, false), "call mum"));
        assert_eq!(parse("remind me at a quarter to eight to leave"), remind(clock(7, 45, None, false), "leave"));
        assert_eq!(
            parse("remind me at seven thirty to feed the cat"),
            remind(clock(7, 30, None, false), "feed the cat")
        );
        assert_eq!(parse("remind me at twenty to eight to leave"), remind(clock(7, 40, None, false), "leave"));
        assert_eq!(
            parse("Remind me tomorrow at 8 to call the dentist."),
            remind(clock(8, 0, None, true), "call the dentist")
        );
        assert_eq!(
            parse("remind me tomorrow morning at 8 to call the dentist"),
            remind(clock(8, 0, Some(false), true), "call the dentist")
        );
        assert_eq!(
            parse("remind me at 9 in the evening to lock up"),
            remind(clock(9, 0, Some(true), false), "lock up")
        );
        assert_eq!(parse("Minn meg på å ringe mamma klokka 7."), remind(clock(7, 0, None, false), "ringe mamma"));
        assert_eq!(
            parse("minn meg på å ringe tannlegen i morgen klokka 8"),
            remind(clock(8, 0, None, true), "ringe tannlegen")
        );
        assert_eq!(
            parse("minn meg på det klokka halv åtte"),
            Some(Remind { when: clock(7, 30, None, false), message: None })
        );
        // "At" before something else is part of the message.
        assert_eq!(parse("remind me to look at the oven in 10 minutes"), remind(Some(In(600)), "look at the oven"));
    }

    #[test]
    fn times_of_day_come_next() {
        let at = |hour: i8, said: Option<When>, alarm: bool| {
            let Some(At(clock)) = said else { panic!("{said:?}") };
            let now = local(hour, 0);
            let next = clock.next(&now, alarm).unwrap();
            (next.date() != now.date(), next.strftime("%H:%M").to_string())
        };
        // No am or pm: whichever comes first.
        assert_eq!(at(14, clock(5, 0, None, false), false), (false, "17:00".into()));
        assert_eq!(at(18, clock(5, 0, None, false), false), (true, "05:00".into()));
        assert_eq!(at(18, clock(5, 0, Some(true), false), false), (true, "17:00".into()));
        assert_eq!(at(9, clock(17, 30, None, false), false), (false, "17:30".into()));
        assert_eq!(at(23, clock(8, 0, None, true), false), (true, "08:00".into()));
        assert_eq!(at(10, clock(3, 0, None, true), false), (true, "15:00".into()));
        assert_eq!(at(10, clock(12, 0, None, false), false), (false, "12:00".into()));
        // An alarm is in the morning.
        assert_eq!(at(14, clock(7, 0, None, false), true), (true, "07:00".into()));
        assert_eq!(at(5, clock(7, 0, None, false), true), (false, "07:00".into()));
    }

    #[test]
    fn a_reminder_at_five_and_its_answer() {
        let t0 = Instant::now();
        let mut timers = timers_at(t0, 14, 0);
        let (text, _) = timers.run(parse("remind me at 5 to call mum").unwrap(), Lang::English, t0);
        assert_eq!(text, "Okay, I'll remind you. That's at 17:00.");
        assert_eq!(timers.next_deadline(), Some(t0 + Duration::from_secs(3 * 3600)));
        let done = timers.due(t0 + Duration::from_secs(3 * 3600));
        assert_eq!(finished_text(&done[0].0), "Reminder: call mum.");
        // Asked when, answered with a time of day.
        let mut timers = timers_at(t0, 14, 0);
        assert_eq!(
            timers.run(parse("remind me to call mum").unwrap(), Lang::English, t0).0,
            "When should I remind you?"
        );
        assert!(timers.answer_pending("banana", Lang::English, t0).is_none());
        let (text, _) = timers.answer_pending("At 5.", Lang::English, t0).unwrap();
        assert_eq!(text, "Okay, I'll remind you. That's at 17:00.");
        for (answer, said) in [
            ("5 pm", "at 17:00"),
            ("half past six", "at 18:30"),
            ("tomorrow at 8", "tomorrow at 08:00"),
            ("at 17:30", "at 17:30"),
        ] {
            let mut timers = timers_at(t0, 14, 0);
            timers.run(Remind { when: None, message: None }, Lang::English, t0);
            let (text, _) = timers.answer_pending(answer, Lang::English, t0).unwrap();
            assert_eq!(text, format!("Okay, I'll remind you. That's {said}."), "{answer}");
        }
        let mut timers = timers_at(t0, 14, 0);
        timers.run(Remind { when: None, message: None }, Lang::English, t0);
        assert_eq!(
            timers.answer_pending("in an hour", Lang::English, t0).unwrap().0,
            "Okay, I'll remind you in 1 hour."
        );
        let mut timers = timers_at(t0, 22, 0);
        let (text, _) = timers.run(parse("minn meg på å ringe mamma klokka 7").unwrap(), Lang::Norwegian, t0);
        assert_eq!(text, "Ok, jeg minner deg på det. Det blir i morgen klokka 07:00.");
    }

    #[test]
    fn timer_names_after_the_length() {
        let named = |seconds: u32, name: &str| Some(Start { seconds: Some(seconds), name: Some(name.to_owned()) });
        assert_eq!(parse("set a timer for 10 minutes for the pasta"), named(600, "pasta"));
        assert_eq!(parse("Set a 2 minute timer for the eggs."), named(120, "eggs"));
        assert_eq!(parse("set a timer for the pizza for 12 minutes"), named(720, "pizza"));
        assert_eq!(parse("set a timer for 1 hour 30"), start(5400));
        assert_eq!(parse("set a timer for an hour and 15"), start(4500));
        assert_eq!(parse("set a timer for 2 hours 45 minutes"), start(9900));
        // Norwegian "to" is two, not minutes.
        assert_eq!(
            parse("remind me in 1 hour to call mum"),
            Some(Remind { when: Some(In(3600)), message: Some("call mum".into()) })
        );
        assert_eq!(parse("set a timer for five minutes"), start(300));
        assert_eq!(parse("stop the timer for now"), Some(Cancel(Which::default())));
    }

    #[test]
    fn wake_up_alarms() {
        let alarm = Which { alarm: true, ..Which::default() };
        assert_eq!(parse("Wake me up at 7."), Some(Alarm { when: clock(7, 0, None, false) }));
        assert_eq!(parse("set an alarm for 6:30"), Some(Alarm { when: clock(6, 30, None, false) }));
        assert_eq!(parse("set an alarm for seven"), Some(Alarm { when: clock(7, 0, None, false) }));
        assert_eq!(parse("wake me up tomorrow at 6 am"), Some(Alarm { when: clock(6, 0, Some(false), true) }));
        assert_eq!(parse("wake me up in 20 minutes"), Some(Alarm { when: Some(In(1200)) }));
        assert_eq!(parse("set an alarm for 10 minutes"), Some(Alarm { when: Some(In(600)) }));
        assert_eq!(parse("set an alarm"), Some(Alarm { when: None }));
        assert_eq!(parse("vekk meg klokka 7"), Some(Alarm { when: clock(7, 0, None, false) }));
        assert_eq!(parse("sett vekkerklokka på halv sju"), Some(Alarm { when: clock(6, 30, None, false) }));
        assert_eq!(parse("cancel my alarm"), Some(Cancel(alarm.clone())));
        assert_eq!(parse("avbryt vekkerklokka"), Some(Cancel(alarm.clone())));
        assert_eq!(parse("Snooze."), Some(Snooze));
        assert_eq!(parse("snooze the alarm"), Some(Snooze));
        assert_eq!(parse("turn off the alarm"), Some(StopAlarm));
        assert_eq!(parse("Stop the alarm."), Some(StopAlarm));
        assert_eq!(parse("is my alarm set"), Some(Remaining(alarm.clone())));
        assert_eq!(parse("When is my alarm?"), Some(Remaining(alarm.clone())));
        assert_eq!(parse("what time is my alarm"), Some(Remaining(alarm)));
        // The house's alarm system.
        for text in [
            "arm the alarm",
            "is the alarm armed",
            "disarm the alarm",
            "is the alarm on",
            "Is the alarm set?",
            "is the house alarm on",
            "slå på alarmen",
        ] {
            assert_eq!(parse(text), None, "{text}");
            assert!(matches!(crate::intent::parse(text), crate::intent::Intent::House(_)), "{text}");
        }
        for (text, command) in [
            ("Stop the alarm.", StopAlarm),
            ("Snooze.", Snooze),
            ("Is my alarm set?", Remaining(Which { alarm: true, ..Which::default() })),
        ] {
            assert_eq!(crate::intent::parse(text), crate::intent::Intent::Timer(command), "{text}");
        }
    }

    #[test]
    fn an_alarm_rings_snoozes_and_stops() {
        let t0 = Instant::now();
        let mut timers = timers_at(t0, 23, 0);
        let (text, notices) = timers.run(parse("wake me up at 7").unwrap(), Lang::English, t0);
        assert_eq!(text, "Okay, your alarm is set. It goes off tomorrow at 07:00.");
        assert_eq!(notices[0].kind, NoticeKind::Started);
        // Timers leave the alarm alone.
        assert_eq!(timers.run(Remaining(Which::default()), Lang::English, t0).0, "There are no timers running.");
        assert_eq!(timers.run(Cancel(Which::default()), Lang::English, t0).0, "There are no timers running.");
        let (text, _) = timers.run(parse("when is my alarm").unwrap(), Lang::English, t0);
        assert_eq!(text, "Your alarm goes off tomorrow at 07:00.");
        assert!(!timers.alarm_ringing(t0));
        let morning = t0 + Duration::from_secs(8 * 3600);
        let done = timers.due(morning);
        assert_eq!(finished_text(&done[0].0), "Time to wake up.");
        assert_eq!(done[0].1[0].kind, NoticeKind::Finished);
        assert!(timers.alarm_ringing(morning));
        let (text, notices) = timers.run(Snooze, Lang::English, morning + Duration::from_secs(30));
        assert_eq!(text, "Okay, snoozing for 9 minutes.");
        assert_eq!(notices[0].total_seconds, SNOOZE);
        let again = morning + Duration::from_secs(30 + SNOOZE as u64);
        assert_eq!(timers.due(again).len(), 1);
        assert_eq!(timers.run(StopAlarm, Lang::English, again).0, "Okay, I've stopped the alarm.");
        assert_eq!(timers.run(Snooze, Lang::English, again).0, "There's no alarm to snooze.");
        assert_eq!(timers.run(StopAlarm, Lang::English, again).0, "There's no alarm ringing.");
        // "Turn off the alarm" when none rings cancels the one set.
        timers.run(parse("set an alarm for 6:30").unwrap(), Lang::English, again);
        assert_eq!(timers.run(StopAlarm, Lang::English, again).0, "The 06:30 alarm is cancelled.");
        assert_eq!(timers.run(parse("is my alarm set").unwrap(), Lang::English, again).0, "You have no alarm set.");
        let (text, _) = timers.run(parse("wake me up in 20 minutes").unwrap(), Lang::English, again);
        assert_eq!(text, "Okay, your alarm is set. It goes off in 20 minutes.");
        let (text, _) = timers.run(Alarm { when: None }, Lang::English, again);
        assert_eq!(text, "For what time?");
        let (text, _) = timers.answer_pending("half past seven", Lang::English, again).unwrap();
        assert_eq!(text, "Okay, your alarm is set. It goes off at 07:30.");
        assert!(ENGLISH_ANSWERS.iter().all(|a| crate::speech_cache::CachedTts::cacheable(a)));
    }

    #[test]
    fn timers_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("innestemme-timers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("timers.json");
        let _ = std::fs::remove_file(&path);
        let t0 = Instant::now();
        let mut timers = Timers::restore(path.clone(), t0, Some((t0, local(14, 0))));
        timers.run(parse("set a pasta timer for 8 minutes").unwrap(), Lang::English, t0);
        timers.run(parse("set a timer for 1 hour").unwrap(), Lang::English, t0);
        timers.run(parse("remind me in 20 minutes to call mum").unwrap(), Lang::English, t0);
        timers.run(parse("set a timer for 2 minutes for the eggs").unwrap(), Lang::English, t0);
        timers.run(parse("wake me up at 14:25").unwrap(), Lang::English, t0);
        timers.run(Pause(Which { name: Some("eggs".into()), ..Which::default() }), Lang::English, t0);
        assert_eq!(timers.list().len(), 5);
        // Down from 14:00 to 14:30, in a new process.
        let t1 = t0 + Duration::from_secs(5);
        let mut back = Timers::restore(path.clone(), t1, Some((t1, local(14, 30))));
        let labels: Vec<String> = back.list().iter().map(|t| t.label(Lang::English)).collect();
        // The pasta ended 22 minutes ago: dropped.
        assert_eq!(labels, ["the 1 hour timer", "the reminder to call mum", "the eggs timer", "the 14:25 alarm"]);
        assert_eq!(back.list()[0].left(t1), Duration::from_secs(30 * 60));
        assert_eq!(back.list()[2].left(t1), Duration::from_secs(120));
        // A satellite connects: what it may still show goes, what runs is shown, and the late ones are told.
        let notices = back.connected(t1);
        assert_eq!(notices[0], Notice { kind: NoticeKind::Cancelled, ..notices[0].clone() });
        assert_eq!(notices[0].id, "timer-1");
        let done = back.due(t1);
        let told: Vec<String> = done.iter().map(|(t, _)| finished_text(t)).collect();
        assert_eq!(told, ["Reminder: call mum. It was due at 14:20.", "Time to wake up."]);
        assert_eq!(done[1].1.iter().map(|n| n.kind).collect::<Vec<_>>(), [NoticeKind::Started, NoticeKind::Finished]);
        // A timer that ended a minute before is still told; ids go on after the kept ones.
        let t2 = t1 + Duration::from_secs(1);
        back.run(parse("set a timer for 1 minute").unwrap(), Lang::English, t2);
        assert_eq!(back.list().last().unwrap().id, 6);
        let t3 = t2 + Duration::from_secs(5);
        let mut again = Timers::restore(path.clone(), t3, Some((t3, local(14, 32))));
        again.connected(t3);
        let done = again.due(t3);
        assert_eq!(finished_text(&done[0].0), "Your 1 minute timer ended while I was restarting.");
        assert_eq!(done[0].1[0].kind, NoticeKind::Cancelled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tomorrow_said_after_midnight_is_the_coming_morning() {
        let Some(At(clock)) = clock(7, 0, None, true) else { unreachable!() };
        let next = clock.next(&local(0, 30), true).unwrap();
        assert_eq!((next.date(), next.strftime("%H:%M").to_string()), (local(0, 30).date(), "07:00".into()));
        // From five on, tomorrow is the next day again.
        let next = clock.next(&local(5, 0), true).unwrap();
        assert_ne!(next.date(), local(5, 0).date());
        let t0 = Instant::now();
        let mut timers = timers_at(t0, 0, 30);
        let (text, _) = timers.run(parse("wake me up tomorrow at 7").unwrap(), Lang::English, t0);
        assert_eq!(text, "Okay, your alarm is set. It goes off at 07:00.");
        assert_eq!(timers.next_deadline(), Some(t0 + Duration::from_secs(6 * 3600 + 1800)));
    }

    #[test]
    fn a_day_further_off_is_named_by_its_weekday() {
        let now = local(14, 0);
        let in_two_days = now.checked_add(jiff::SignedDuration::from_hours(48)).unwrap();
        assert_eq!(say_at(&in_two_days, &now, Lang::English), "on Monday at 14:00");
        assert_eq!(say_at(&in_two_days, &now, Lang::Norwegian), "på mandag klokka 14:00");
        let tomorrow = now.checked_add(jiff::SignedDuration::from_hours(24)).unwrap();
        assert_eq!(say_at(&tomorrow, &now, Lang::English), "tomorrow at 14:00");
    }

    #[test]
    fn a_duration_after_in_beats_an_hour_in_the_message() {
        assert_eq!(
            parse("Remind me in 10 minutes to call Anna at 3"),
            Some(Remind { when: Some(In(600)), message: Some("call anna at 3".into()) })
        );
        // The time said first wins.
        assert!(matches!(
            parse("remind me at 6 to leave in 20 minutes"),
            Some(Remind { when: Some(At(Clock { hour: 6, minute: 0, .. })), .. })
        ));
        let t0 = Instant::now();
        let mut timers = timers_at(t0, 14, 0);
        timers.run(Remind { when: None, message: None }, Lang::English, t0);
        let (text, _) = timers.answer_pending("in an hour and 15", Lang::English, t0).unwrap();
        assert_eq!(text, "Okay, I'll remind you in 1 hour and 15 minutes.");
    }

    #[test]
    fn alarm_clock_questions_are_the_wake_up_alarm() {
        let alarm = Some(Remaining(Which { alarm: true, ..Which::default() }));
        for text in
            ["Is the alarm clock set?", "is there an alarm set", "Er vekkerklokka stilt?", "okay, is my alarm set"]
        {
            assert_eq!(parse(text), alarm, "{text}");
        }
        // Filler first still reaches the house alarm.
        for text in ["Okay, is the alarm on?", "Hey, is the alarm set?"] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn moving_the_alarm_that_is_set() {
        assert_eq!(
            parse("move my alarm to 7:30"),
            Some(MoveAlarm(Move::To(At(Clock { hour: 7, minute: 30, pm: None, tomorrow: false }))))
        );
        assert_eq!(
            parse("change my alarm to 7"),
            Some(MoveAlarm(Move::To(At(Clock { hour: 7, minute: 0, pm: None, tomorrow: false }))))
        );
        assert_eq!(parse("push my alarm back 10 minutes"), Some(MoveAlarm(Move::By(600))));
        assert_eq!(parse("add 10 minutes to my alarm"), Some(MoveAlarm(Move::By(600))));
        assert_eq!(parse("delay my alarm by an hour"), Some(MoveAlarm(Move::By(3600))));
        assert_eq!(parse("move my alarm 10 minutes earlier"), Some(MoveAlarm(Move::By(-600))));
        assert_eq!(parse("move my alarm forward 15 minutes"), Some(MoveAlarm(Move::By(-900))));
        assert_eq!(
            parse("postpone my alarm until 8"),
            Some(MoveAlarm(Move::To(At(Clock { hour: 8, minute: 0, pm: None, tomorrow: false }))))
        );
        assert_eq!(
            parse("flytt vekkerklokka til 8"),
            Some(MoveAlarm(Move::To(At(Clock { hour: 8, minute: 0, pm: None, tomorrow: false }))))
        );
        // No way said: never now plus 15 minutes in place of the alarm.
        assert_eq!(parse("change my alarm by 15 minutes"), None);
        // A new alarm, not the one that is set.
        assert_eq!(parse("add an alarm for 7"), Some(Alarm { when: clock(7, 0, None, false) }));
        for (text, seconds) in [
            ("add an alarm in 10 minutes", 600),
            ("set one more alarm in 10 minutes", 600),
            ("set an extra alarm for 20 minutes", 1200),
            ("legg inn en vekkerklokke om 20 minutter", 1200),
            ("set an alarm for half an hour later", 1800),
        ] {
            assert_eq!(parse(text), Some(Alarm { when: Some(In(seconds)) }), "{text}");
        }
        assert_eq!(parse("set a later alarm at 9"), Some(Alarm { when: clock(9, 0, None, false) }));
        let t0 = Instant::now();
        let mut timers = timers_at(t0, 23, 0);
        assert_eq!(timers.run(MoveAlarm(Move::By(600)), Lang::English, t0).0, "You have no alarm set.");
        timers.run(parse("wake me up at 7").unwrap(), Lang::English, t0);
        let (text, notices) = timers.run(parse("move my alarm to 7:30").unwrap(), Lang::English, t0);
        assert_eq!(text, "Okay, your alarm is moved. It goes off tomorrow at 07:30.");
        assert_eq!(notices.iter().map(|n| n.kind).collect::<Vec<_>>(), [NoticeKind::Cancelled, NoticeKind::Started]);
        let (text, _) = timers.run(parse("push my alarm back 10 minutes").unwrap(), Lang::English, t0);
        assert_eq!(text, "Okay, your alarm is moved. It goes off tomorrow at 07:40.");
        assert_eq!(timers.list().len(), 1);
        assert_eq!(timers.list()[0].label(Lang::English), "the 07:40 alarm");
        let (text, _) = timers.run(parse("move my alarm 15 minutes earlier").unwrap(), Lang::English, t0);
        assert_eq!(text, "Okay, your alarm is moved. It goes off tomorrow at 07:25.");
        assert_eq!(
            timers.run(MoveAlarm(Move::By(-9 * 3600)), Lang::English, t0).0,
            "Sorry, that time has already passed."
        );
        assert_eq!(timers.list()[0].label(Lang::English), "the 07:25 alarm");
        // While it rings, later is a snooze that long.
        let morning = t0 + Duration::from_secs(8 * 3600 + 25 * 60);
        assert_eq!(timers.due(morning).len(), 1);
        assert!(timers.list().is_empty());
        let (text, notices) = timers.run(parse("add 10 minutes to my alarm").unwrap(), Lang::English, morning);
        assert_eq!(text, "Okay, snoozing for 10 minutes.");
        assert_eq!(notices[0].total_seconds, 600);
        assert_eq!(timers.list().len(), 1);
        timers.run(Cancel(Which { alarm: true, ..Which::default() }), Lang::English, morning);
        // Two alarms: which one.
        timers.run(parse("wake me up at 7").unwrap(), Lang::English, t0);
        timers.run(parse("wake me up at 9").unwrap(), Lang::English, t0);
        let (text, notices) = timers.run(parse("move my alarm to 8").unwrap(), Lang::English, t0);
        assert!(text.ends_with("Which one?"), "{text}");
        assert!(notices.is_empty());
        assert_eq!(timers.list().len(), 2);
        assert!(ENGLISH_ANSWERS.contains(&"Okay, your alarm is moved."));
    }

    #[test]
    fn a_lone_alarm_word_is_not_a_request_yet() {
        assert_eq!(parse("alarm"), None);
        assert_eq!(parse("the alarm"), None);
        for text in [
            "show my alarms",
            "when's my alarm",
            "how long until my alarm",
            "check my alarm",
            "alarm status",
            "my alarm",
        ] {
            assert_eq!(parse(text), Some(Remaining(Which { alarm: true, ..Which::default() })), "{text}");
        }
        assert_eq!(parse("I need an alarm"), Some(Alarm { when: None }));
    }

    #[test]
    fn a_bad_kept_entry_drops_only_itself() {
        let dir = std::env::temp_dir().join(format!("innestemme-timers-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("timers.json");
        let t0 = Instant::now();
        let good = r#"{"id": 7, "total_seconds": 600, "left_seconds": 300}"#;
        std::fs::write(&path, format!("[{good}, {{\"id\": \"seven\"}}]")).unwrap();
        let mut timers = Timers::restore(path.clone(), t0, Some((t0, local(14, 0))));
        assert_eq!(timers.list().len(), 1);
        timers.run(start(60).unwrap(), Lang::English, t0);
        assert_eq!(timers.list()[1].id, 8);
        // Not a list at all: set aside, not overwritten by the next save.
        std::fs::write(&path, "{ broken").unwrap();
        let timers = Timers::restore(path.clone(), t0, Some((t0, local(14, 0))));
        assert!(timers.list().is_empty());
        assert_eq!(std::fs::read_to_string(path.with_extension("bad")).unwrap(), "{ broken");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn which_one_is_kept_safe() {
        let t0 = Instant::now();
        let two_timers = |timers: &mut Timers| {
            timers.run(parse("set a pasta timer for 10 minutes").unwrap(), Lang::English, t0);
            timers.run(parse("set an egg timer for 5 minutes").unwrap(), Lang::English, t0);
            let (text, _) = timers.run(parse("cancel the timer").unwrap(), Lang::English, t0);
            assert!(text.ends_with("Which one?"), "{text}");
        };
        // Both ended before the answer: no panic, and nothing cancelled silently.
        let mut timers = timers_at(t0, 12, 0);
        two_timers(&mut timers);
        let later = t0 + Duration::from_secs(5);
        timers.list.clear();
        assert_eq!(timers.answer_pending("both", Lang::English, later).unwrap().0, "That one has already ended.");
        two_timers(&mut timers);
        timers.list.clear();
        assert_eq!(
            timers.answer_pending("the last one", Lang::English, later).unwrap().0,
            "That one has already ended."
        );
        // A position is the list as it was said, even after one ended.
        two_timers(&mut timers);
        timers.list.remove(0);
        let (text, _) = timers.answer_pending("the second one", Lang::English, later).unwrap();
        assert_eq!(text, "The egg timer is cancelled.");
        // Too late, or a request of its own: a new request, nothing cancelled.
        for (answer, at) in [
            ("the first one", t0 + Duration::from_secs(40)),
            ("turn off all the lights", later),
            ("nei, glem det, alle er fine", later),
            ("set another timer for 10 minutes", later),
            ("remind me at 7 to call the first one", later),
            ("the", later),
            ("one", later),
            ("volume 7", later),
            ("lights at 7", later),
            ("wait 5 minutes", later),
            ("pause the pasta one", later),
            ("cancel the pasta timer", later),
        ] {
            let mut timers = timers_at(t0, 12, 0);
            two_timers(&mut timers);
            assert!(timers.answer_pending(answer, Lang::English, at).is_none(), "{answer}");
            assert_eq!(timers.list().len(), 2, "{answer}");
        }
        // A request with its own verb is not taken as the answer, and goes past the room filter
        // only as itself.
        let mut timers = timers_at(t0, 12, 0);
        two_timers(&mut timers);
        assert!(timers.takes_answer("the pasta one"));
        assert!(!timers.takes_answer("pause the pasta one"));
        assert!(!timers.takes_answer("with a 40 year tariff for murdering his barn"));
        // The question goes once its window has passed.
        timers.forget_stale(t0 + Duration::from_secs(10));
        assert!(timers.awaiting_duration());
        timers.forget_stale(t0 + Duration::from_secs(31));
        assert!(!timers.awaiting_duration());
        // A Norwegian compound name.
        let mut timers = timers_at(t0, 12, 0);
        two_timers(&mut timers);
        let (text, _) = timers.answer_pending("pastatimeren", Lang::English, later).unwrap();
        assert_eq!(text, "The pasta timer is cancelled.");
        // Of 07:00 and 19:00, "the 7 o'clock one" is the one that comes next.
        let mut timers = timers_at(t0, 12, 0);
        timers.run(parse("set an alarm for 7 pm").unwrap(), Lang::English, t0);
        timers.run(parse("add an alarm for 7 am").unwrap(), Lang::English, t0);
        timers.run(parse("cancel my alarm").unwrap(), Lang::English, t0);
        let (text, _) = timers.answer_pending("the 7 o'clock one", Lang::English, t0).unwrap();
        assert_eq!(text, "The 19:00 alarm is cancelled.");
        // Said with its half of the day, against the 24-hour time.
        for (answer, cancelled) in [
            ("the 7 pm one", "The 19:00 alarm is cancelled."),
            ("the 7 in the evening", "The 19:00 alarm is cancelled."),
            ("den 7 om kvelden", "The 19:00 alarm is cancelled."),
            ("the 7 am one", "The 07:00 alarm is cancelled."),
        ] {
            let mut timers = timers_at(t0, 4, 0);
            timers.run(parse("set an alarm for 7 pm").unwrap(), Lang::English, t0);
            timers.run(parse("add an alarm for 7 am").unwrap(), Lang::English, t0);
            timers.run(parse("cancel my alarm").unwrap(), Lang::English, t0);
            let (text, _) = timers.answer_pending(answer, Lang::English, t0).unwrap();
            assert_eq!(text, cancelled, "{answer}");
        }
        // After a move, a reply with another verb does not move anything.
        let mut timers = timers_at(t0, 22, 0);
        timers.run(parse("wake me up at 6").unwrap(), Lang::English, t0);
        timers.run(parse("add an alarm for 7").unwrap(), Lang::English, t0);
        timers.run(parse("move my alarm to 7:30").unwrap(), Lang::English, t0);
        assert!(timers.answer_pending("delete the 6 o'clock one", Lang::English, t0).is_none());
        let labels: Vec<String> = timers.list().iter().map(|t| t.label(Lang::English)).collect();
        assert_eq!(labels, ["the 06:00 alarm", "the 07:00 alarm"]);
        // A move takes one.
        let mut timers = timers_at(t0, 22, 0);
        timers.run(parse("wake me up at 6").unwrap(), Lang::English, t0);
        timers.run(parse("add an alarm for 7").unwrap(), Lang::English, t0);
        timers.run(parse("move my alarm to 7:30").unwrap(), Lang::English, t0);
        assert!(timers.choosing());
        let (text, _) = timers.answer_pending("both of them", Lang::English, t0).unwrap();
        assert_eq!(text, "I can only move one at a time.");
        assert_eq!(timers.list().len(), 2);
    }

    #[test]
    fn which_one_is_answered() {
        let t0 = Instant::now();
        let mut timers = timers_at(t0, 22, 0);
        timers.run(parse("wake me up at 6").unwrap(), Lang::English, t0);
        timers.run(parse("add an alarm for 7").unwrap(), Lang::English, t0);
        let (text, _) = timers.run(parse("move my alarm to 7:30").unwrap(), Lang::English, t0);
        assert!(text.ends_with("Which one?"), "{text}");
        assert!(timers.awaiting_duration());
        let (text, notices) = timers.answer_pending("the 7 o'clock one", Lang::English, t0).unwrap();
        assert_eq!(text, "Okay, your alarm is moved. It goes off tomorrow at 07:30.");
        assert_eq!(notices.len(), 2);
        let labels: Vec<String> = timers.list().iter().map(|t| t.label(Lang::English)).collect();
        assert_eq!(labels, ["the 06:00 alarm", "the 07:30 alarm"]);
        // Cancel asks the same and takes "the first one".
        let (text, _) = timers.run(parse("cancel my alarm").unwrap(), Lang::English, t0);
        assert!(text.ends_with("Which one?"), "{text}");
        let (text, _) = timers.answer_pending("the first one", Lang::English, t0).unwrap();
        assert_eq!(text, "The 06:00 alarm is cancelled.");
        assert_eq!(timers.list().len(), 1);
        assert!(!timers.awaiting_duration());
        // An answer that names none of them is a new request.
        timers.run(parse("set a pasta timer for 10 minutes").unwrap(), Lang::English, t0);
        timers.run(parse("set an egg timer for 5 minutes").unwrap(), Lang::English, t0);
        let (text, _) = timers.run(parse("cancel the timer").unwrap(), Lang::English, t0);
        assert!(text.ends_with("Which one?"), "{text}");
        assert!(timers.answer_pending("what's the weather", Lang::English, t0).is_none());
        assert!(!timers.awaiting_duration());
        timers.run(parse("cancel the timer").unwrap(), Lang::English, t0);
        let (text, _) = timers.answer_pending("the pasta one", Lang::English, t0).unwrap();
        assert_eq!(text, "The pasta timer is cancelled.");
        timers.run(parse("set a pasta timer for 10 minutes").unwrap(), Lang::English, t0);
        timers.run(parse("cancel the timer").unwrap(), Lang::English, t0);
        let (text, _) = timers.answer_pending("both", Lang::English, t0).unwrap();
        assert_eq!(text, "All 2 timers are cancelled.");
    }

    #[test]
    fn a_count_is_not_a_time() {
        let Some(Remind { when, message }) = parse("remind me to look at 2 houses tomorrow") else { panic!() };
        assert_eq!(when, None);
        assert_eq!(message.as_deref(), Some("look at 2 houses tomorrow"));
        let Some(Remind { when, .. }) = parse("remind me at 2 this afternoon to call Anna") else { panic!() };
        assert_eq!(when, clock(2, 0, Some(true), false));
        // Only a counted noun right after the number: other words keep the time.
        assert_eq!(parse("wake me at 6 thanks"), Some(Alarm { when: clock(6, 0, None, false) }));
        assert_eq!(parse("set an alarm at 7 weekdays"), Some(Alarm { when: clock(7, 0, None, false) }));
        assert_eq!(parse("set an alarm for 7 tuesdays"), Some(Alarm { when: clock(7, 0, None, false) }));
        let Some(Remind { when, .. }) = parse("remind me at 4 thanks") else { panic!() };
        assert_eq!(when, clock(4, 0, None, false));
        // After "at" alone the number is still the time.
        let Some(Remind { when, .. }) = parse("remind me at 5 people are coming") else { panic!() };
        assert_eq!(when, clock(5, 0, None, false));
    }

    #[test]
    fn minutes_around_half_in_norwegian() {
        for (text, hour, minute) in [
            ("vekk meg klokka fem over halv åtte", 7, 35),
            ("vekk meg klokka fem på halv åtte", 7, 25),
            ("vekk meg klokka ti over halv sju", 6, 40),
            ("vekk meg klokka kvart på halv ni", 8, 15),
            // Around one o'clock the usual 12-hour rule applies, not a fixed 00:35.
            ("vekk meg klokka fem over halv ett", 12, 35),
            ("vekk meg klokka halv ett", 12, 30),
        ] {
            assert_eq!(parse(text), Some(Alarm { when: clock(hour, minute, None, false) }), "{text}");
        }
        let Some(Remind { when: Some(At(at)), .. }) = parse("minn meg på det klokka fem over halv ett") else {
            panic!()
        };
        assert_eq!(at.next(&local(9, 0), false).unwrap().hour(), 12);
        assert_eq!(at.next(&local(13, 0), false).unwrap().hour(), 0);
    }
}

//! Timers and reminders: durations from speech (English and Norwegian, digits or words), the
//! running timers, and what the assistant says about them.
//!
//! A reminder is a timer with a message: when it ends, the message is spoken instead of a ring.

use std::time::{Duration, Instant};

use crate::lang::Lang;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerCommand {
    /// `seconds` is `None` when the request names no duration ("set a timer"): ask for it.
    Start {
        seconds: Option<u32>,
        name: Option<String>,
    },
    /// "Remind me in 10 minutes to check the oven"; the message can be missing.
    Remind {
        seconds: Option<u32>,
        message: Option<String>,
    },
    Cancel(Which),
    Pause(Which),
    Resume(Which),
    Add {
        seconds: u32,
        which: Which,
    },
    /// How much time is left; also "what timers do I have".
    Remaining(Which),
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
}

const TIMER_WORDS: &[&str] = &["timer", "timers", "timeren", "timerne", "countdown", "nedtelling", "nedtellingen"];
const REMINDER_WORDS: &[&str] = &["reminder", "reminders", "påminnelse", "påminnelsen", "påminnelser", "påminnelsene"];
const CANCEL_WORDS: &[&str] =
    &["cancel", "stop", "delete", "remove", "clear", "off", "avbryt", "stopp", "slett", "fjern"];
const PAUSE_WORDS: &[&str] = &["pause", "hold", "freeze"];
const RESUME_WORDS: &[&str] = &["resume", "unpause", "continue", "fortsett", "gjenoppta"];
const ADD_WORDS: &[&str] = &["add", "extend", "extra", "more", "legg", "forleng", "forlenge"];
const START_WORDS: &[&str] = &["set", "start", "make", "create", "new", "put", "sett", "lag", "starte", "ny"];
const LEFT_WORDS: &[&str] = &["left", "remaining", "remain", "igjen", "gjenstår"];
/// Words right before "timer" that are not its name.
const NOT_NAMES: &[&str] = &[
    "a", "an", "the", "my", "this", "that", "one", "another", "new", "any", "all", "every", "of", "for", "on", "set",
    "start", "cancel", "stop", "pause", "resume", "delete", "remove", "clear", "add", "is", "your", "en", "ei", "et",
    "ett", "den", "det", "min", "mi", "mitt", "ny", "alle", "på", "til", "sett", "lag", "avbryt", "stopp", "slett",
    "fjern", "ingen", "noen", "hvilke", "which", "what",
];

/// Lower-case words with digits kept ("1.5"), hyphens split ("10-minute").
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == '-')
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, '.' | ',' | '\''))
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
    let (seconds, used) = duration_words(&words);
    let remind = words.windows(2).any(|p| matches!((p[0].as_str(), p[1].as_str()), ("remind", "me") | ("minn", "meg")));
    if remind {
        return Some(TimerCommand::Remind { seconds, message: reminder_text(&words, &used) });
    }
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
        seconds: None,
        all: has("all") || has("alle") || has("every") || has("everything"),
        reminder,
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

/// The duration a request names, in seconds; `None` when it names none. For answers to "For how
/// long?".
pub fn duration(text: &str) -> Option<u32> {
    duration_words(&tokens(text)).0
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
        if half_follows(words, end) {
            amount += unit / 2.0;
            end += 3;
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
            if let Some(ones) = words.get(j).and_then(|w| word_number(w)).filter(|o| (1..10).contains(o)) {
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
    // "a timer called pasta", "en timer som heter pasta"
    if let Some(i) = words.iter().position(|w| matches!(w.as_str(), "called" | "named" | "heter" | "kalt")) {
        let name: Vec<&str> = (i + 1..words.len())
            .take_while(|&k| is_free(k) && !matches!(words[k].as_str(), "for" | "please" | "på" | "takk"))
            .map(|k| words[k].as_str())
            .collect();
        if !name.is_empty() {
            return Some(name.join(" "));
        }
    }
    let first = words.iter().position(|w| is_timer_word(w))?;
    if let Some(stem) = compound_name(&words[first]) {
        return Some(stem.to_owned());
    }
    let before = first.checked_sub(1)?;
    (is_free(before) && !NOT_NAMES.contains(&words[before].as_str())).then(|| words[before].clone())
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

    /// "the pasta timer", "the 10 minute timer", "the reminder to call mom"; Norwegian
    /// "pasta-timeren", "timeren på 10 minutter", "påminnelsen".
    pub fn label(&self, lang: Lang) -> String {
        let total = self.total.as_secs() as u32;
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
        if which.reminder && !self.is_reminder {
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

/// A start that is waiting for its duration ("Set a timer." "For how long?").
#[derive(Debug, Clone)]
struct PendingStart {
    name: Option<String>,
    reminder: Option<String>,
    is_reminder: bool,
}

#[derive(Default)]
pub struct Timers {
    list: Vec<Timer>,
    next_id: u32,
    pending: Option<PendingStart>,
}

impl Timers {
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

    /// Removes and returns the timers that have ended, with their notices.
    pub fn due(&mut self, now: Instant) -> Vec<(Timer, Notice)> {
        let (done, running): (Vec<Timer>, Vec<Timer>) = std::mem::take(&mut self.list)
            .into_iter()
            .partition(|t| matches!(t.state, State::Running { ends } if ends <= now));
        self.list = running;
        done.into_iter()
            .map(|t| {
                let notice = Notice::of(&t, NoticeKind::Finished, now);
                (t, notice)
            })
            .collect()
    }

    /// True when the last answer asked "For how long?".
    pub fn awaiting_duration(&self) -> bool {
        self.pending.is_some()
    }

    /// Forgets a question that was not answered.
    pub fn clear_pending(&mut self) {
        self.pending = None;
    }

    /// The answer to "For how long?": starts the pending timer when `text` names a duration.
    pub fn answer_pending(&mut self, text: &str, lang: Lang, now: Instant) -> Option<(String, Vec<Notice>)> {
        let seconds = duration(text)?;
        let pending = self.pending.take()?;
        Some(self.start(seconds, pending.name, pending.reminder, pending.is_reminder, lang, now))
    }

    pub fn run(&mut self, command: TimerCommand, lang: Lang, now: Instant) -> (String, Vec<Notice>) {
        self.pending = None;
        let no = lang == Lang::Norwegian;
        match command {
            TimerCommand::Start { seconds: None, name } => {
                self.pending = Some(PendingStart { name, reminder: None, is_reminder: false });
                (if no { "Hvor lenge?" } else { "For how long?" }.into(), vec![])
            }
            TimerCommand::Start { seconds: Some(seconds), name } => self.start(seconds, name, None, false, lang, now),
            TimerCommand::Remind { seconds: None, message } => {
                self.pending = Some(PendingStart { name: None, reminder: message, is_reminder: true });
                (if no { "Når skal jeg minne deg på det?" } else { "When should I remind you?" }.into(), vec![])
            }
            TimerCommand::Remind { seconds: Some(seconds), message } => {
                self.start(seconds, None, message, true, lang, now)
            }
            TimerCommand::Remaining(which) => (self.remaining(&which, lang, now), vec![]),
            TimerCommand::Cancel(which) => self.change(&which, lang, now, Change::Cancel),
            TimerCommand::Pause(which) => self.change(&which, lang, now, Change::Pause),
            TimerCommand::Resume(which) => self.change(&which, lang, now, Change::Resume),
            TimerCommand::Add { seconds, which } => self.change(&which, lang, now, Change::Add(seconds)),
        }
    }

    fn start(
        &mut self,
        seconds: u32,
        name: Option<String>,
        reminder: Option<String>,
        is_reminder: bool,
        lang: Lang,
        now: Instant,
    ) -> (String, Vec<Notice>) {
        self.next_id += 1;
        let total = Duration::from_secs(seconds as u64);
        let timer = Timer {
            id: self.next_id,
            name,
            reminder,
            lang,
            total,
            state: State::Running { ends: now + total },
            is_reminder,
        };
        let length = say_duration(seconds, lang);
        let text = match (lang, &timer.name, timer.is_reminder) {
            (Lang::English, _, true) => format!("Okay, I'll remind you in {length}."),
            (Lang::English, Some(name), false) => {
                format!("{} timer set for {length}.", capitalize(name))
            }
            (Lang::English, None, false) => format!("Timer set for {length}."),
            (Lang::Norwegian, _, true) => format!("Ok, jeg minner deg på det om {length}."),
            (Lang::Norwegian, Some(name), false) => {
                format!("{}-timer satt på {length}.", capitalize(name))
            }
            (Lang::Norwegian, None, false) => format!("Timer satt på {length}."),
        };
        let notice = Notice::of(&timer, NoticeKind::Started, now);
        self.list.push(timer);
        (text, vec![notice])
    }

    /// The timers a request means. A name that matches nothing still means the only timer there
    /// is: names get misheard, and the language model invents them ("the pizza").
    fn select(&self, which: &Which) -> Vec<usize> {
        let found: Vec<usize> = (0..self.list.len()).filter(|&i| self.list[i].matches(which)).collect();
        if found.is_empty() && self.list.len() == 1 && which.seconds.is_none() && !which.reminder {
            return vec![0];
        }
        found
    }

    fn remaining(&self, which: &Which, lang: Lang, now: Instant) -> String {
        let no = lang == Lang::Norwegian;
        let mut found: Vec<&Timer> = self.select(which).into_iter().map(|i| &self.list[i]).collect();
        found.sort_by_key(|t| t.left(now));
        let describe = |t: &Timer, single: bool| {
            let left = say_left(t.left(now), lang);
            let label = t.label(lang);
            match (no, t.is_running(), single) {
                (false, true, true) => format!("{} left on {label}", capitalize(&left)),
                (false, true, false) => format!("{label} has {left} left"),
                (false, false, _) => format!("{label} is paused, with {left} left"),
                (true, true, true) => format!("{} igjen på {label}", capitalize(&left)),
                (true, true, false) => format!("{label} har {left} igjen"),
                (true, false, _) => format!("{label} står på pause, med {left} igjen"),
            }
        };
        match found.as_slice() {
            [] if self.list.is_empty() => none_running(no),
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
        let targets = match found.as_slice() {
            [] if self.list.is_empty() => return (none_running(no), vec![]),
            [] => return (not_found(which, no), vec![]),
            [_] => found,
            several if which.all => several.to_vec(),
            several => {
                let labels: Vec<String> = several.iter().map(|&i| self.list[i].label(lang)).collect();
                let text = if no {
                    format!("Du har {} timere: {}. Hvilken?", several.len(), join(&labels, no))
                } else {
                    format!("You have {} timers: {}. Which one?", several.len(), join(&labels, no))
                };
                return (text, vec![]);
            }
        };
        let mut notices = Vec::new();
        let count = targets.len();
        let label = self.list[targets[0]].label(lang);
        for &i in targets.iter().rev() {
            let timer = &mut self.list[i];
            let kind = match change {
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
        let text = match (no, change) {
            (false, Change::Cancel) if count > 1 => format!("All {count} timers are cancelled."),
            (true, Change::Cancel) if count > 1 => format!("Alle {count} timerne er avbrutt."),
            (false, Change::Pause) if count > 1 => format!("All {count} timers are paused."),
            (true, Change::Pause) if count > 1 => format!("Alle {count} timerne er satt på pause."),
            (false, Change::Resume) if count > 1 => {
                format!("All {count} timers are running again.")
            }
            (true, Change::Resume) if count > 1 => format!("Alle {count} timerne går igjen."),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Cancel,
    Pause,
    Resume,
    Add(u32),
}

fn left_of(notices: &[Notice], lang: Lang) -> String {
    say_left(Duration::from_secs(notices.first().map_or(0, |n| n.seconds_left) as u64), lang)
}

fn none_running(no: bool) -> String {
    if no { "Ingen timere er i gang." } else { "There are no timers running." }.into()
}

fn not_found(which: &Which, no: bool) -> String {
    match (&which.name, no) {
        (Some(name), false) => format!("I couldn't find a {name} timer."),
        (Some(name), true) => format!("Jeg fant ingen timer som heter {name}."),
        (None, false) => "I couldn't find that timer.".into(),
        (None, true) => "Jeg fant ikke den timeren.".into(),
    }
}

/// What to say when a timer ends.
pub fn finished_text(timer: &Timer) -> String {
    match (timer.lang, &timer.reminder, timer.is_reminder) {
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
        let reminder =
            |seconds: Option<u32>, message: &str| Some(Remind { seconds, message: Some(message.to_owned()) });
        assert_eq!(parse("Remind me in 10 minutes to check the oven."), reminder(Some(600), "check the oven"));
        assert_eq!(parse("remind me to call my mother in an hour"), reminder(Some(3600), "call your mother"));
        assert_eq!(parse("Minn meg på å ringe mamma om en time."), reminder(Some(3600), "ringe mamma"));
        assert_eq!(parse("remind me to water the plants"), reminder(None, "water the plants"));
        assert_eq!(parse("remind me in five minutes"), Some(Remind { seconds: Some(300), message: None }));
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
        assert_eq!(text, "6 minutes and 40 seconds left on the 10 minute timer.");
        assert!(timers.due(t0 + Duration::from_secs(599)).is_empty());
        let done = timers.due(t0 + Duration::from_secs(600));
        assert_eq!(done.len(), 1);
        assert_eq!(finished_text(&done[0].0), "Your 10 minute timer is done.");
        assert_eq!(done[0].1.kind, NoticeKind::Finished);
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
        assert_eq!(text, "9 minutes left on the 10 minute timer.");
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
}

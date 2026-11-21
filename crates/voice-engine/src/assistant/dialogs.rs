//! Multi-turn dialogs through a real [`Worker`]: a question the assistant asks back ("Which room?",
//! "For how long?", "Arm the alarm?") and the reply that answers it, as the listener hears them.
//!
//! [`Worker::for_test`] builds a worker with no network service but an optional [`FakeHa`]: no
//! speech, no language model, no weather or departures. [`Dialog::say`] passes a text to
//! `Worker::heard` as the listener's transcript and returns the answer the worker spoke.
//!
//! The tests state what the worker does now. A known bug has its test written for the right
//! behaviour and `#[ignore]`d with the reason, until the fix lands.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;

use serde_json::{json, Value};

use super::conversation::{CONFIRM_WINDOW, ROOM_WINDOW};
use super::*;

/// Speech output that says nothing: the dialog is read from the answer events.
struct Silent;

impl Tts for Silent {
    fn speak(&mut self, _: &str, _: Lang, _: &mut dyn FnMut(&[i16])) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A listener that hears nothing: the tests hand the worker its transcripts.
struct Deaf;

impl Transcribe for Deaf {
    fn transcribe(&mut self, _: &[i16]) -> anyhow::Result<Vec<Transcript>> {
        Ok(Vec::new())
    }
}

/// A listener that hears the same text in everything.
struct Hears(&'static str);

impl Transcribe for Hears {
    fn transcribe(&mut self, _: &[i16]) -> anyhow::Result<Vec<Transcript>> {
        Ok(vec![Transcript { lang: Lang::English, text: self.0.to_owned(), score: 0.0 }])
    }
}

/// What the fake Home Assistant has, and the service calls made to it.
#[derive(Default)]
struct House {
    lights: Value,
    scenes: Value,
    /// The shopping list's items, as `todo.get_items` returns them.
    items: Vec<Value>,
    calls: Vec<String>,
}

/// Home Assistant on a local port: the templates the worker renders for lights, scenes and to-do
/// lists, and service calls, which are recorded. Other templates render as an empty list.
pub(super) struct FakeHa {
    url: String,
    house: Arc<Mutex<House>>,
}

impl FakeHa {
    /// A kitchen and a living room, each with a light and a "Relax" scene, and an empty shopping list.
    pub(super) fn new() -> Self {
        let house = House {
            lights: json!([
                { "id": "light.kitchen", "name": "Kitchen", "area": "Kitchen", "group": false, "members": [] },
                { "id": "light.sofa", "name": "Sofa", "area": "Living Room", "group": false, "members": [] },
            ]),
            scenes: json!([
                { "id": "scene.kitchen_relax", "name": "Relax", "area": "Kitchen" },
                { "id": "scene.living_room_relax", "name": "Relax", "area": "Living Room" },
            ]),
            ..House::default()
        };
        let house = Arc::new(Mutex::new(house));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let shared = house.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let _ = serve(stream, &shared);
            }
        });
        Self { url, house }
    }

    pub(super) fn client(&self) -> HomeAssistant {
        HomeAssistant::new(&self.url, "test")
    }

    /// The service calls so far, `domain.service {data}`, and none after.
    pub(super) fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut self.house.lock().unwrap().calls)
    }
}

/// One request on one connection, then the connection closes.
fn serve(stream: std::net::TcpStream, house: &Mutex<House>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let data: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (status, reply) = answer(&path, data, &mut house.lock().unwrap());
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    )
}

fn answer(path: &str, data: Value, house: &mut House) -> (&'static str, String) {
    if path == "/api/template" {
        let template = data["template"].as_str().unwrap_or_default();
        let rendered = if template.contains("states.light if") {
            house.lights.to_string()
        } else if template.contains("states.scene") {
            house.scenes.to_string()
        } else if template.contains("states.todo") {
            json!([{ "id": "todo.shopping_list", "name": "Shopping list" }]).to_string()
        } else {
            "[]".to_owned()
        };
        return ("200 OK", rendered);
    }
    let Some(service) = path.strip_prefix("/api/services/") else { return ("404 Not Found", "{}".into()) };
    let (service, response) = match service.split_once('?') {
        Some((service, _)) => (service.replace('/', "."), true),
        None => (service.replace('/', "."), false),
    };
    house.calls.push(format!("{service} {data}"));
    match service.as_str() {
        "todo.get_items" => {
            ("200 OK", json!({ "service_response": { "todo.shopping_list": { "items": house.items } } }).to_string())
        }
        "todo.add_item" => {
            let uid = house.items.len().to_string();
            house.items.push(json!({ "summary": data["item"], "uid": uid, "status": "needs_action" }));
            ("200 OK", "[]".into())
        }
        _ if response => ("200 OK", json!({ "service_response": {} }).to_string()),
        _ => ("200 OK", "[]".into()),
    }
}

impl Worker {
    /// A worker for tests: no speech, listener, language model, cache or saved files, and Home
    /// Assistant only when given (a [`FakeHa`] client). Also the answer events and the reply ring's
    /// other end, which must live as long as the worker.
    pub(super) fn for_test(
        home_assistant: Option<HomeAssistant>,
    ) -> (Self, broadcast::Receiver<AssistantEvent>, rtrb::Consumer<i16>) {
        let config = AssistantConfig {
            wake_listener: None,
            home: None,
            address: None,
            transit_stops: Vec::new(),
            contact: None,
            news_feeds: (Vec::new(), Vec::new()),
            price_area: None,
            home_assistant: None,
            wake: WakeWord::new("Jarvis"),
            llm: None,
            speaker: None,
            room: None,
            honorific: None,
            honorific_female: None,
            voice_gender: None,
            renderer: None,
            often_spoken: Vec::new(),
            dump_utterances: None,
            unhandled: None,
            learned: None,
            timers_file: None,
            memory_file: None,
            tv: None,
            plex: None,
            screen_url: None,
            alerts: false,
            bedtime_light: 20,
            home_room: None,
            home_music: None,
            lockdown_music: String::new(),
            lockdown_volume: 50,
            open_source_music: String::new(),
            party: Party::new(None),
            norwegian_groceries: None,
            activity: Arc::default(),
        };
        let (events, received) = broadcast::channel(256);
        let (reply, consumer) = rtrb::RingBuffer::new(SAMPLE_RATE as usize);
        let agent = "innestemme-test";
        let worker = Worker {
            events,
            reply,
            busy: Arc::default(),
            interrupted: Arc::default(),
            pause: Arc::default(),
            screen: Arc::default(),
            tts: Box::new(Silent),
            config,
            weather: Weather::new(agent),
            transit: Transit::new(agent, None, Vec::new()),
            power: None,
            news: News::new(agent, &[], &[]),
            lookup: Lookup::new(agent),
            live: HashSet::new(),
            home_location: None,
            jokes: Jokes::default(),
            home_assistant,
            player: Player::new(None),
            tv: Tv::new(None),
            transcriber: Box::new(Deaf),
            name_checker: None,
            wake_check: None,
            conversation: Conversation::default(),
            said: VecDeque::new(),
            speech_ended: None,
            last_lights: None,
            shopping: ShoppingList::default(),
            last_list: None,
            request_audio: Vec::new(),
            turn: None,
            timers: Timers::default(),
            memory: Memory::default(),
            notes_said: Vec::new(),
            previewing: false,
            request_addressed: false,
            persona: None,
            render: None,
            lead_turn: 0,
            chat_turn: 0,
            last_audio: None,
            room: None,
            warmed_prompt: String::new(),
            lockdown_volume: None,
            party_under_lockdown: false,
            party_restore: None,
            locked_down: false,
            light_undo_gone: None,
            lockdown_tv: None,
            open_source_party: false,
            threat_turn: 0,
            alarm: None,
            refreshed: None,
        };
        (worker, received, consumer)
    }
}

/// A conversation with a test worker.
pub(super) struct Dialog {
    pub(super) worker: Worker,
    events: broadcast::Receiver<AssistantEvent>,
    _reply: rtrb::Consumer<i16>,
}

impl Dialog {
    pub(super) fn new(ha: Option<&FakeHa>) -> Self {
        let (worker, events, reply) = Worker::for_test(ha.map(FakeHa::client));
        Self { worker, events, _reply: reply }
    }

    /// `text` heard in English, as the listener's only transcript; the answer spoken, if any.
    pub(super) fn say(&mut self, text: &str) -> Option<String> {
        self.say_in(Lang::English, text)
    }

    pub(super) fn say_in(&mut self, lang: Lang, text: &str) -> Option<String> {
        let heard = [Transcript { lang, text: text.to_owned(), score: 0.0 }];
        let now = Instant::now();
        self.worker.heard(&heard, &[], now, now, false);
        self.answers()
    }

    /// `text` heard in English in a draft, the first pause of `audio`; the answer spoken, if the
    /// draft was acted on.
    pub(super) fn draft(&mut self, text: &str, audio: &[i16]) -> Option<String> {
        let heard = vec![Transcript { lang: Lang::English, text: text.to_owned(), score: 0.0 }];
        self.worker.conversation.draft = Some((audio.to_vec(), heard));
        self.worker.act_on_draft();
        self.answers()
    }

    /// The answers spoken since the last call, one after another.
    pub(super) fn answers(&mut self) -> Option<String> {
        let mut answers = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if let AssistantEvent::Answer { text } = event {
                answers.push(text);
            }
        }
        (!answers.is_empty()).then(|| answers.join(" "))
    }

    /// A request answered while nobody listens, as a background refresh does it (`data_sentences`,
    /// the web page's preview): composed with `previewing` set, nothing spoken.
    pub(super) fn compose_aside(&mut self, text: &str) -> Option<String> {
        self.worker.previewing = true;
        let answer = self.worker.compose(text, Lang::English);
        self.worker.previewing = false;
        answer
    }
}

fn said(answer: Option<String>) -> String {
    answer.unwrap_or_else(|| "<nothing said>".into())
}

#[test]
fn heard_tells_whether_words_will_follow() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    let mut words = |text: &str| {
        let heard = [Transcript { lang: Lang::English, text: text.to_owned(), score: 0.0 }];
        let now = Instant::now();
        dialog.worker.heard(&heard, &[], now, now, false);
        let mut flag = None;
        let mut spoken = false;
        while let Ok(event) = dialog.events.try_recv() {
            match event {
                AssistantEvent::Heard { words, .. } => flag = Some(words),
                AssistantEvent::Answer { .. } => spoken = true,
                _ => {}
            }
        }
        (flag, spoken)
    };
    assert_eq!(words("Jarvis, turn on the kitchen lights"), (Some(true), true));
    assert_eq!(words("Jarvis, what's 5 plus 5"), (Some(true), true));
    assert_eq!(words("Jarvis, pause the music and turn on the kitchen lights"), (Some(true), true));
    // May end quietly: done without words, "never mind", and a request no rule takes (a language
    // model, when there is one, may ignore it). The flag errs toward false.
    // Nothing plays in the test, so it says so: the flag cannot know that ahead.
    assert_eq!(words("Jarvis, pause the music"), (Some(false), true));
    assert_eq!(words("Jarvis, never mind"), (Some(false), false));
    assert_eq!(words("Jarvis, purple monkey dishwasher"), (Some(false), true));
}

#[test]
fn a_light_request_without_a_room_asks_which_and_takes_the_room() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, turn on the lights")), "Which room?");
    assert!(ha.take_calls().is_empty());
    // Said without the name, in the follow-up window.
    assert_eq!(said(dialog.say("the kitchen")), "Okay, the kitchen lights are on.");
    assert_eq!(ha.take_calls(), [r#"light.turn_on {"entity_id":["light.kitchen"]}"#]);
}

#[test]
fn a_new_request_drops_the_light_room_question() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, turn off the lights")), "Which room?");
    assert_eq!(said(dialog.say("Jarvis, what time is it")), time_of_day(Lang::English));
    // The question is gone: a room alone, without the name, is the room talking.
    assert_eq!(dialog.say("the kitchen"), None);
    assert!(ha.take_calls().is_empty());
}

#[test]
fn a_scene_room_said_with_the_name_answers_which_room() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, activate the relax scene")), "Which room?");
    assert_eq!(said(dialog.say("Jarvis, the kitchen")), "Okay, relax.");
    assert_eq!(ha.take_calls(), [r#"scene.turn_on {"entity_id":"scene.kitchen_relax"}"#]);
}

#[test]
fn a_scene_room_said_without_the_name_answers_which_room() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, activate the relax scene")), "Which room?");
    assert_eq!(said(dialog.say("the kitchen")), "Okay, relax.");
    assert_eq!(ha.take_calls(), [r#"scene.turn_on {"entity_id":"scene.kitchen_relax"}"#]);
}

#[test]
fn arming_the_alarm_waits_for_yes() {
    let mut dialog = Dialog::new(None);
    assert_eq!(said(dialog.say("Jarvis, arm the alarm")), "Arm the alarm?");
    assert!(dialog.worker.conversation.pending_confirm.is_some());
    // "Yes" without the name reaches the alarm, here a missing Home Assistant.
    assert_eq!(said(dialog.say("yes")), "Home Assistant isn't connected yet.");
    assert!(dialog.worker.conversation.pending_confirm.is_none());
}

#[test]
fn no_leaves_the_alarm_and_other_words_are_the_room() {
    let mut dialog = Dialog::new(None);
    assert_eq!(said(dialog.say("Jarvis, turn on privacy mode")), "Turn on privacy mode?");
    // Not a yes or a no, without the name: the room talking, and the question still waits.
    assert_eq!(dialog.say("we should get going"), None);
    assert!(dialog.worker.conversation.pending_confirm.is_some());
    assert_eq!(said(dialog.say("no")), "Okay, I'll leave it.");
    assert!(dialog.worker.conversation.pending_confirm.is_none());
}

#[test]
fn the_shopping_list_takes_items_while_open() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, add to the shopping list")), "What should I add?");
    assert!(dialog.worker.conversation.list_is_open());
    let answer = said(dialog.say("milk"));
    assert!(answer.starts_with("Added milk."), "{answer}");
    // More items, without "add" or the name, while the list is open.
    let answer = said(dialog.say("eggs and butter"));
    assert!(answer.starts_with("Added eggs and butter."), "{answer}");
    assert_eq!(said(dialog.say("no, that's it")), "Okay, that's everything.");
    assert!(!dialog.worker.conversation.list_is_open());
    let added: Vec<String> = ha.take_calls().into_iter().filter(|c| c.starts_with("todo.add_item")).collect();
    assert_eq!(added.len(), 3, "{added:?}");
}

/// A request's audio up to a pause, and the same with the rest of the closing silence.
fn draft_audio() -> (Vec<i16>, Vec<i16>) {
    let draft = vec![1000i16; 20 * FRAME_SAMPLES];
    let mut whole = draft.clone();
    whole.resize(draft.len() + (vad::END_FRAMES - vad::DRAFT_FRAMES) as usize * FRAME_SAMPLES, 0);
    (draft, whole)
}

#[test]
fn a_whole_request_is_answered_from_its_draft() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    let (draft, whole) = draft_audio();
    let answer = said(dialog.draft("Jarvis, what's on the shopping list?", &draft));
    assert!(answer.contains("shopping list is empty"), "{answer}");
    // The utterance's own end, 0.4 s later, is the same request: not answered again.
    dialog.worker.utterance(&whole, Instant::now());
    assert_eq!(dialog.answers(), None);
}

#[test]
fn a_request_that_goes_on_after_its_draft_is_answered_again() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    let (draft, _) = draft_audio();
    assert!(dialog.draft("Jarvis, what's on the shopping list?", &draft).is_some());
    // The speaker went on after the pause: the longer utterance is a second request, and the
    // first answer stands. The cost of a draft acted on too soon.
    let mut longer = draft.clone();
    longer.resize(draft.len() + 30 * FRAME_SAMPLES, 1000);
    dialog.worker.transcriber = Box::new(Hears("Jarvis, what's on the shopping list for the weekend?"));
    dialog.worker.utterance(&longer, Instant::now());
    let answer = said(dialog.answers());
    assert!(answer.contains("shopping list is empty"), "{answer}");
}

#[test]
fn a_request_that_may_go_on_waits_for_its_end() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    let (draft, _) = draft_audio();
    // "... in the kitchen", "... and eggs" may follow.
    for text in ["Jarvis, turn on the lights", "Jarvis, add milk", "Jarvis, what's on the shopping list and"] {
        assert_eq!(dialog.draft(text, &draft), None, "{text}");
        assert!(dialog.worker.conversation.draft.is_some(), "{text}: the draft waits for the utterance");
        assert!(ha.take_calls().is_empty(), "{text}");
    }
}

#[test]
fn a_timer_without_a_length_asks_for_one() {
    let mut dialog = Dialog::new(None);
    assert_eq!(said(dialog.say("Jarvis, set a timer")), "For how long?");
    assert!(dialog.worker.timers.awaiting_duration());
    let answer = said(dialog.say("ten minutes"));
    assert!(answer.contains("10 minutes"), "{answer}");
    assert!(!dialog.worker.timers.awaiting_duration());
}

#[test]
fn which_timer_takes_one_of_those_listed() {
    let mut dialog = Dialog::new(None);
    said(dialog.say("Jarvis, set a pasta timer for 10 minutes"));
    said(dialog.say("Jarvis, set an egg timer for 5 minutes"));
    let question = said(dialog.say("Jarvis, cancel the timer"));
    assert!(question.ends_with("Which one?"), "{question}");
    assert!(dialog.worker.timers.choosing());
    // A reply that names none of them is no answer: without the name it is the room.
    assert_eq!(dialog.say("okay"), None);
    assert!(dialog.worker.timers.choosing());
    let answer = said(dialog.say("the pasta one"));
    assert!(answer.to_lowercase().contains("pasta"), "{answer}");
    assert!(!dialog.worker.timers.choosing());
    let left = said(dialog.say("Jarvis, how much time is left"));
    assert_eq!(left, "5 minutes left.");
}

#[test]
fn remember_what_takes_the_next_words_as_the_note() {
    let mut dialog = Dialog::new(None);
    assert_eq!(said(dialog.say("Jarvis, remember something")), "Remember what?");
    assert!(dialog.worker.memory.asked_what());
    // Without the name and with no language model to judge it, words no rule knows are the room.
    assert_eq!(dialog.say("the spare key is in the kitchen drawer"), None);
    assert_eq!(said(dialog.say("Jarvis, remember something")), "Remember what?");
    let answer = said(dialog.say("Jarvis, the spare key is in the kitchen drawer"));
    assert!(answer.starts_with("Okay"), "{answer}");
    let answer = said(dialog.say("Jarvis, where is the spare key"));
    assert!(answer.contains("kitchen drawer"), "{answer}");
}

#[test]
fn forgetting_everything_waits_for_yes() {
    let mut dialog = Dialog::new(None);
    said(dialog.say("Jarvis, remember that the bins go out on Tuesday"));
    assert_eq!(said(dialog.say("Jarvis, forget everything")), "Forget everything I remember?");
    assert!(dialog.worker.memory.asked_to_forget());
    let answer = said(dialog.say("yes"));
    assert!(answer.starts_with("Okay"), "{answer}");
    let answer = said(dialog.say("Jarvis, what do you remember"));
    assert!(!answer.contains("bins"), "{answer}");
}

#[test]
fn a_preview_keeps_remember_what() {
    let mut dialog = Dialog::new(None);
    assert_eq!(said(dialog.say("Jarvis, remember something")), "Remember what?");
    // The web page's preview and a refresh ahead of time compose with `previewing` set.
    assert_eq!(said(dialog.compose_aside("what time is it")), time_of_day(Lang::English));
    assert!(dialog.worker.memory.asked_what());
    let answer = said(dialog.say("Jarvis, the spare key is in the kitchen drawer"));
    assert!(answer.starts_with("Okay"), "{answer}");
}

#[test]
fn a_refresh_keeps_which_room_for_the_lights() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, turn on the lights")), "Which room?");
    said(dialog.compose_aside("what time is it"));
    assert_eq!(said(dialog.say("the kitchen")), "Okay, the kitchen lights are on.");
}

#[test]
fn a_refresh_keeps_which_room_for_a_scene() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, activate the relax scene")), "Which room?");
    said(dialog.compose_aside("what time is it"));
    assert_eq!(said(dialog.say("Jarvis, the kitchen")), "Okay, relax.");
}

#[test]
fn a_refresh_keeps_a_question_waiting_for_yes() {
    let mut dialog = Dialog::new(None);
    assert_eq!(said(dialog.say("Jarvis, arm the alarm")), "Arm the alarm?");
    said(dialog.compose_aside("what time is it"));
    assert!(dialog.worker.conversation.pending_confirm.is_some());
}

#[test]
fn a_refresh_keeps_the_shopping_list_open() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, add to the shopping list")), "What should I add?");
    said(dialog.compose_aside("what time is it"));
    assert!(dialog.worker.conversation.list_is_open());
}

#[test]
fn a_refresh_keeps_for_how_long() {
    let mut dialog = Dialog::new(None);
    assert_eq!(said(dialog.say("Jarvis, set a timer")), "For how long?");
    said(dialog.compose_aside("what time is it"));
    assert!(dialog.worker.timers.awaiting_duration());
}

#[test]
fn a_web_button_keeps_which_room() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, turn on the lights")), "Which room?");
    assert_eq!(dialog.worker.typed("what time is it"), time_of_day(Lang::English));
    assert!(dialog.worker.conversation.pending_lights.is_some());
}

#[test]
fn a_preview_of_a_read_request_keeps_which_room() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, turn on the lights")), "Which room?");
    // The web page's "hear the answer" for a request that switches something is not run at all.
    assert!(matches!(dialog.worker.preview("turn on the kitchen lights"), Preview::NotRun(_)));
    assert_eq!(said(dialog.say("the kitchen")), "Okay, the kitchen lights are on.");
}

#[test]
fn norwegian_lights_ask_which_room_in_norwegian() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say_in(Lang::Norwegian, "Jarvis, skru på lyset")), "Hvilket rom?");
    let answer = said(dialog.say_in(Lang::Norwegian, "kjøkkenet"));
    assert!(answer.starts_with("Ok"), "{answer}");
    assert_eq!(ha.take_calls(), [r#"light.turn_on {"entity_id":["light.kitchen"]}"#]);
}

/// A question asked `by` earlier than it was.
fn asked_earlier(at: &mut Instant, by: Duration) {
    *at -= by;
}

#[test]
fn which_room_for_the_lights_goes_after_its_window() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, turn on the lights")), "Which room?");
    let (_, at) = dialog.worker.conversation.pending_lights.as_mut().unwrap();
    asked_earlier(at, ROOM_WINDOW + Duration::from_secs(1));
    assert_eq!(dialog.worker.awaiting(), None);
    // Even with the name, a room said long after is not the answer.
    assert_eq!(said(dialog.say("Jarvis, the kitchen")), "Sorry, I can't help with that yet.");
    assert!(ha.take_calls().is_empty());
    assert!(dialog.worker.conversation.pending_lights.is_none());
}

#[test]
fn which_room_for_a_scene_goes_after_its_window() {
    let ha = FakeHa::new();
    let mut dialog = Dialog::new(Some(&ha));
    assert_eq!(said(dialog.say("Jarvis, activate the relax scene")), "Which room?");
    let (_, at) = dialog.worker.conversation.pending_scene.as_mut().unwrap();
    asked_earlier(at, ROOM_WINDOW + Duration::from_secs(1));
    assert_eq!(dialog.worker.awaiting(), None);
    assert_eq!(said(dialog.say("Jarvis, the kitchen")), "Sorry, I can't help with that yet.");
    assert!(ha.take_calls().iter().all(|call| !call.starts_with("scene.")));
}

#[test]
fn a_yes_after_the_confirm_window_is_the_room() {
    let mut dialog = Dialog::new(None);
    assert_eq!(said(dialog.say("Jarvis, arm the alarm")), "Arm the alarm?");
    let (_, at) = dialog.worker.conversation.pending_confirm.as_mut().unwrap();
    asked_earlier(at, CONFIRM_WINDOW + Duration::from_secs(1));
    assert_eq!(dialog.worker.awaiting(), None);
    // In the follow-up window, without the name: no longer taken as the reply.
    assert_eq!(dialog.say("yes"), None);
}

#[test]
fn never_mind_drops_every_question() {
    let ha = FakeHa::new();
    for asks in [
        "Jarvis, turn on the lights",
        "Jarvis, activate the relax scene",
        "Jarvis, arm the alarm",
        "Jarvis, add to the shopping list",
        "Jarvis, set a timer",
        "Jarvis, remember something",
    ] {
        let mut dialog = Dialog::new(Some(&ha));
        said(dialog.say(asks));
        assert!(dialog.worker.awaiting().is_some(), "{asks}");
        // Ends quietly, or is the reply ("Okay, I'll leave it."): either way nothing waits on.
        dialog.say("Jarvis, never mind");
        assert_eq!(dialog.worker.awaiting(), None, "{asks}");
        let conversation = &dialog.worker.conversation;
        assert!(conversation.pending_lights.is_none() && conversation.pending_scene.is_none(), "{asks}");
        assert!(conversation.pending_confirm.is_none() && conversation.list_open.is_none(), "{asks}");
    }
}

#[test]
fn an_answer_that_ends_the_conversation_takes_the_closed_follow_up_with_it() {
    let mut dialog = Dialog::new(None);
    // As a skill that closes the window (party mode) and then ends without an answer.
    dialog.worker.conversation.follow_up_closed = true;
    assert_eq!(dialog.say("Jarvis, never mind"), None);
    assert!(!dialog.worker.conversation.follow_up());
    assert!(!dialog.worker.conversation.follow_up_closed);
    // The next answer opens its window as usual.
    assert_eq!(said(dialog.say("Jarvis, what time is it")), time_of_day(Lang::English));
    assert!(dialog.worker.conversation.follow_up());
}

#[test]
fn an_interrupt_with_nothing_asked_after_it_is_cleared() {
    let mut dialog = Dialog::new(None);
    // The wake word said over an answer, and then nothing.
    dialog.worker.interrupted.store(true, Relaxed);
    dialog.worker.woken(Instant::now(), WakeName::Heard);
    dialog.worker.end_turn(TurnEnd::Silent);
    assert!(!dialog.worker.interrupted.load(Relaxed));
    assert!(dialog.worker.conversation.wake.is_none());
}

#[test]
fn every_question_asked_back_holds_the_lead_in_and_goes_with_a_new_request() {
    let ha = FakeHa::new();
    let weather = "what's the weather";
    let setups: [(&[&str], Awaiting); 8] = [
        (&["Jarvis, turn on the lights"], Awaiting::LightsRoom),
        (&["Jarvis, activate the relax scene"], Awaiting::SceneRoom),
        (&["Jarvis, arm the alarm"], Awaiting::Confirm),
        (&["Jarvis, add to the shopping list"], Awaiting::ListItems),
        (&["Jarvis, set a timer"], Awaiting::TimerDuration),
        (
            &[
                "Jarvis, set a pasta timer for 10 minutes",
                "Jarvis, set an egg timer for 5 minutes",
                "Jarvis, cancel the timer",
            ],
            Awaiting::WhichTimer,
        ),
        (&["Jarvis, remember something"], Awaiting::RememberWhat),
        (&["Jarvis, remember that the bins go out on Tuesday", "Jarvis, forget everything"], Awaiting::Forget),
    ];
    for (asks, awaiting) in setups {
        let mut dialog = Dialog::new(Some(&ha));
        dialog.worker.render = Some(RenderAhead::spawn(Box::new(Silent)).unwrap());
        assert!(dialog.worker.lead_in(weather, Lang::English).is_some(), "a lead-in when nothing is asked");
        for ask in asks {
            said(dialog.say(ask));
        }
        assert_eq!(dialog.worker.awaiting(), Some(awaiting), "{asks:?}");
        assert_eq!(dialog.worker.lead_in(weather, Lang::English), None, "{asks:?}");
        // A preview leaves it; a new request said to the assistant drops it.
        said(dialog.compose_aside("what time is it"));
        assert_eq!(dialog.worker.awaiting(), Some(awaiting), "{asks:?}");
        assert_eq!(said(dialog.say("Jarvis, what time is it")), time_of_day(Lang::English), "{asks:?}");
        assert_eq!(dialog.worker.awaiting(), None, "{asks:?}");
    }
}

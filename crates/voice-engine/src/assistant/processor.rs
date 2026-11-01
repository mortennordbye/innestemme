//! The model thread's side: the energy detector, half duplex, and the reply ring.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tracing::{info, warn};
use voice_assistant::geo::{self, Location};
use voice_assistant::ha::HomeAssistant;
use voice_assistant::jokes::Jokes;
use voice_assistant::lang::Lang;
use voice_assistant::lookup::Lookup;
use voice_assistant::memory::Memory;
use voice_assistant::music::Player;
use voice_assistant::news::News;
use voice_assistant::persona::Persona;
use voice_assistant::power::{self, Power};
use voice_assistant::shopping::ShoppingList;
use voice_assistant::timer::Timers;
use voice_assistant::transit::Transit;
use voice_assistant::tts::Tts;
use voice_assistant::tv::Tv;
use voice_assistant::vad::Vad;
use voice_assistant::voice_gender::VoiceGender;
use voice_assistant::weather::Weather;
use voice_assistant::whisper::Transcribe;
use voice_proto::{FRAME_SAMPLES, SAMPLE_RATE};
use voice_rt::noalloc::permit_alloc;

use crate::activity::Activity;
use crate::engine::{FrameProcessor, ProcessError};
use crate::metrics::Metrics;
use crate::party::Party;
use crate::render::{Priority, RenderAhead};
use crate::screen::Screen;

use super::handle::AssistantHandle;
use super::home::watch_alerts;
use super::pause::Pause;
use super::{unreachable_text, AssistantConfig, AssistantEvent, Job, Worker};

/// Covers the STT's 0.5 s delay plus the room's echo tail.
const DEAF_AFTER_REPLY_FRAMES: u64 = 13;

pub struct AssistantProcessor {
    vad: Vad,
    jobs: mpsc::Sender<Job>,
    events: broadcast::Sender<AssistantEvent>,
    reply: rtrb::Consumer<i16>,
    /// Set from dispatch until the reply is queued.
    busy: Arc<AtomicBool>,
    /// A satellite started listening: stop ignoring the microphone at once.
    hear_now: Arc<AtomicBool>,
    interrupted: Arc<AtomicBool>,
    pause: Arc<Pause>,
    screen: Arc<Screen>,
    party: Party,
    activity: Arc<Activity>,
    voice_gender: Option<Arc<VoiceGender>>,
    was_busy: bool,
    frame: u64,
    deaf_until: u64,
}

impl AssistantProcessor {
    /// `transcriber` is Whisper or Parakeet, one call per utterance the energy detector cuts.
    pub fn new(
        transcriber: Box<dyn Transcribe>,
        tts: Box<dyn Tts>,
        mut config: AssistantConfig,
    ) -> anyhow::Result<Self> {
        let wake_listener = config.wake_listener.take();
        let (jobs, rx) = mpsc::channel();
        let (reply_tx, reply) = rtrb::RingBuffer::new(SAMPLE_RATE as usize * 60);
        let busy = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(Pause::default());
        let screen = Arc::new(Screen::default());
        let party = config.party.clone();
        let activity = config.activity.clone();
        let voice_gender = config.voice_gender.clone().filter(|_| config.honorific_female.is_some());
        let speaker = config.speaker.clone();
        let tv = Tv::new(config.tv.clone()).with_plex(config.plex.clone());
        let (events, _) = broadcast::channel(256);
        // MET Norway asks for an application name and a contact.
        let user_agent = match &config.contact {
            Some(contact) => format!("innestemme/{} {contact}", env!("CARGO_PKG_VERSION")),
            None => format!("innestemme/{}", env!("CARGO_PKG_VERSION")),
        };
        let home_location = config.address.as_deref().and_then(|address| {
            match geo::home(&geo::agent(&user_agent), address, config.home.as_deref()) {
                Ok(Some(home)) => {
                    info!(
                        label = home.label,
                        latitude = home.latitude,
                        longitude = home.longitude,
                        "home address found"
                    );
                    Some(home)
                }
                Ok(None) => {
                    warn!("the address was not found; weather and departures use `home` instead");
                    None
                }
                Err(error) => {
                    warn!(error = format!("{error:#}"), "could not look up the address");
                    None
                }
            }
        });
        let transit = Transit::new(&user_agent, home_location.clone(), config.transit_stops.clone());
        let in_norway =
            |home: &Location| (57.5..71.5).contains(&home.latitude) && (4.0..31.5).contains(&home.longitude);
        let price_area = config.price_area.clone().or_else(|| {
            home_location
                .as_ref()
                .filter(|home| in_norway(home))
                .map(|h| power::area_for(h.latitude, h.longitude).into())
        });
        if let Some(area) = &price_area {
            info!(area, "electricity price area");
        }
        let power = price_area.map(|area| Power::new(&user_agent, &area));
        let news = News::new(&user_agent, &config.news_feeds.0, &config.news_feeds.1);
        let shopping = match config.norwegian_groceries {
            Some(_) => ShoppingList::default().with_norwegian_names(),
            None => ShoppingList::default(),
        };
        let timers = config.timers_file.clone().map_or_else(Timers::default, Timers::kept_in);
        let memory = config.memory_file.clone().map_or_else(Memory::default, Memory::kept_in);
        let mut worker = Worker {
            events: events.clone(),
            reply: reply_tx,
            busy: busy.clone(),
            interrupted: interrupted.clone(),
            pause: pause.clone(),
            screen: screen.clone(),
            tts,
            config,
            weather: Weather::new(&user_agent),
            news,
            lookup: Lookup::new(&user_agent),
            live: HashSet::new(),
            transit,
            power,
            home_location,
            jokes: Jokes::default(),
            home_assistant: None,
            player: Player::new(speaker),
            tv,
            transcriber,
            wake_listener,
            draft: None,
            follow_up_until: None,
            woken_at: None,
            wake_named: None,
            said: VecDeque::new(),
            answered_draft: None,
            hushed: None,
            speech_ended: None,
            history: VecDeque::new(),
            last_lights: None,
            pending_lights: None,
            shopping,
            last_list: None,
            list_open: None,
            request_audio: Vec::new(),
            turn: None,
            pending_scene: None,
            pending_confirm: None,
            timers,
            memory,
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
            threat_turn: 0,
            music_under_answer: false,
            after_answer: None,
            after_answer_at: None,
            alarm: None,
            refreshed: None,
        };
        worker.room = worker.config.room.clone();
        worker.persona = worker.config.honorific.as_deref().map(|h| match &worker.config.honorific_female {
            Some(female) if worker.config.voice_gender.is_some() => Persona::new(h).with_others(&[female, ""]),
            _ => Persona::new(h),
        });
        if let Some(renderer) = worker.config.renderer.take() {
            let render = RenderAhead::spawn(renderer)?;
            let (soon, idle) = worker.fixed_sentences();
            info!(
                often = worker.config.often_spoken.len(),
                soon = soon.len(),
                idle = idle.len(),
                "rendering speech ahead"
            );
            render.add(Priority::Soon, std::mem::take(&mut worker.config.often_spoken));
            render.add(Priority::Soon, soon);
            render.add(Priority::Idle, idle);
            worker.render = Some(render);
        }
        if let Some((url, token)) = &worker.config.home_assistant {
            let mut ha = HomeAssistant::new(url, token);
            // Once for the calls of one request: a lockdown makes three.
            let (failed, last) = (jobs.clone(), std::sync::Mutex::new(None::<Instant>));
            ha.write_behind(move |error| {
                let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
                if last.is_none_or(|at| at.elapsed() > Duration::from_secs(10)) {
                    *last = Some(Instant::now());
                    let _ = failed.send(Job::Failed(unreachable_text(error, false).into(), Lang::English));
                }
            })?;
            match ha.check() {
                Ok(lights) => info!(url, lights, "home assistant connected"),
                Err(error) => {
                    warn!(url, %error, "home assistant not usable; light requests will fail")
                }
            }
            let failed = jobs.clone();
            worker.player.on_late_failure(move |text, lang| {
                let _ = failed.send(Job::Failed(text, lang));
            });
            match worker.player.check(&ha) {
                Ok(player) => info!(player, "music assistant connected"),
                Err(error) => warn!(%error, "music assistant not usable; music requests will fail"),
            }
            worker.home_assistant = Some(ha);
            if let Some(render) = worker.render.clone() {
                let home = worker.home_sentences();
                info!(sentences = home.len(), "rendering this home's answers ahead");
                render.add(Priority::Soon, home);
            }
        }
        if let Some((url, token)) = worker.config.home_assistant.clone().filter(|_| worker.config.alerts) {
            let jobs = jobs.clone();
            std::thread::Builder::new()
                .name("alerts".into())
                .spawn(move || watch_alerts(HomeAssistant::new(&url, &token), jobs))?;
            info!("watching home assistant for safety alerts");
        }
        if let Some(llm) = &worker.config.llm {
            let start = Instant::now();
            let system = worker.system_prompt();
            match llm.warm_up(&system) {
                Ok(()) => {
                    info!(model = llm.model(), elapsed = ?start.elapsed(), "language model warmed up");
                    worker.warmed_prompt = system;
                }
                Err(error) => {
                    warn!(%error, "language model not reachable; unrecognised requests get the fallback")
                }
            }
        }
        let start = Instant::now();
        worker.transcriber.transcribe(&[0i16; FRAME_SAMPLES * 12])?;
        if let Some(listener) = &mut worker.wake_listener {
            listener.transcribe(&[0i16; FRAME_SAMPLES * 12])?;
        }
        info!(elapsed = ?start.elapsed(), "listener warmed up");
        std::thread::Builder::new().name("assistant".into()).spawn(move || worker.run(rx))?;
        let hear_now = Arc::new(AtomicBool::new(false));
        Ok(Self {
            vad: Vad::default(),
            jobs,
            events,
            reply,
            busy,
            hear_now,
            interrupted,
            pause,
            screen,
            party,
            activity,
            voice_gender,
            was_busy: false,
            frame: 0,
            deaf_until: 0,
        })
    }

    pub fn handle(&self) -> AssistantHandle {
        AssistantHandle {
            jobs: self.jobs.clone(),
            events: self.events.clone(),
            hear_now: self.hear_now.clone(),
            interrupted: self.interrupted.clone(),
            pause: self.pause.clone(),
            screen: self.screen.clone(),
            party: self.party.clone(),
            activity: self.activity.clone(),
            voice_gender: self.voice_gender.clone(),
        }
    }
}

impl FrameProcessor for AssistantProcessor {
    fn process(&mut self, input: &[i16], output: &mut [i16], metrics: &Metrics) -> Result<(), ProcessError> {
        self.frame += 1;
        if self.hear_now.swap(false, Relaxed) {
            self.deaf_until = 0;
            self.was_busy = false;
            // A satellite plays answers itself; what is left in the ring is not heard anywhere.
            let queued = self.reply.slots();
            if let Ok(chunk) = self.reply.read_chunk(queued) {
                chunk.commit_all();
            }
        }
        // Interrupted by a satellite's wake word: what is still being synthesized is dropped, and the
        // request said over it must be heard.
        let interrupted = self.interrupted.load(Relaxed);
        if interrupted {
            self.deaf_until = 0;
            let queued = self.reply.slots();
            if let Ok(chunk) = self.reply.read_chunk(queued) {
                chunk.commit_all();
            }
        }
        let busy = self.busy.load(Relaxed) && !interrupted;
        if busy || self.was_busy {
            let queued = (self.reply.slots() / FRAME_SAMPLES) as u64;
            self.deaf_until = self.deaf_until.max(self.frame + queued + DEAF_AFTER_REPLY_FRAMES);
        }
        self.was_busy = busy;
        let deaf = self.frame < self.deaf_until;

        let start = Instant::now();
        permit_alloc(|| {
            if deaf {
                self.vad.reset();
                return;
            }
            if let Some(utterance) = self.vad.push(input) {
                info!(seconds = utterance.len() as f32 / SAMPLE_RATE as f32, "utterance");
                let _ = self.jobs.send(Job::Utterance(utterance, Instant::now()));
            }
            if let Some(draft) = self.vad.draft() {
                let _ = self.jobs.send(Job::Draft(draft));
            }
        });
        metrics.engine_step.record(start.elapsed());

        let n = self.reply.slots().min(output.len());
        match self.reply.read_chunk(n) {
            Ok(chunk) => {
                let (a, b) = chunk.as_slices();
                output[..a.len()].copy_from_slice(a);
                output[a.len()..n].copy_from_slice(b);
                output[n..].fill(0);
                chunk.commit_all();
            }
            Err(_) => output.fill(0),
        }
        Ok(())
    }

    fn reset(&mut self) {
        permit_alloc(|| self.vad.reset());
        self.deaf_until = 0;
        let queued = self.reply.slots();
        if let Ok(chunk) = self.reply.read_chunk(queued) {
            chunk.commit_all();
        }
        let _ = permit_alloc(|| self.jobs.send(Job::Ready));
    }
}

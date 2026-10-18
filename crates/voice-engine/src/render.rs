//! Speech rendered ahead of time on a thread of its own, with a voice of its own, so a request never
//! waits for a sentence being rendered. It writes to the speech cache folder the assistant plays
//! from; what is kept there already is skipped without being read.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use voice_assistant::lang::Lang;
use voice_assistant::tts::Tts;

/// A sentence that took longer than this was synthesized, not found kept.
const SYNTHESIZED: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// What is said most, and the answers to the data fetched just now: rendered back to back.
    Soon,
    /// The long tail: after each sentence a rest as long as it took, so the CPU (and the fans) are
    /// not at full tilt for an hour after a start.
    Idle,
}

#[derive(Clone)]
pub struct RenderAhead {
    queue: mpsc::Sender<(Priority, Vec<String>)>,
    hold: Arc<AtomicBool>,
}

impl RenderAhead {
    pub fn spawn(tts: Box<dyn Tts>) -> std::io::Result<Self> {
        let (queue, sentences) = mpsc::channel();
        let hold = Arc::new(AtomicBool::new(false));
        let held = hold.clone();
        std::thread::Builder::new().name("render-ahead".into()).spawn(move || run(tts, &sentences, &held))?;
        Ok(Self { queue, hold })
    }

    pub fn add(&self, priority: Priority, sentences: Vec<String>) {
        if !sentences.is_empty() {
            let _ = self.queue.send((priority, sentences));
        }
    }

    /// While a conversation lasts, the CPU is the request's: no new sentence is started.
    pub fn hold(&self, hold: bool) {
        self.hold.store(hold, Relaxed);
    }
}

fn run(mut tts: Box<dyn Tts>, sentences: &mpsc::Receiver<(Priority, Vec<String>)>, hold: &AtomicBool) {
    let mut queues = Queues::default();
    let mut rest_until: Option<Instant> = None;
    let mut rendered = 0usize;
    loop {
        if queues.is_empty() {
            if rendered > 0 {
                info!(rendered, "speech rendered ahead");
                rendered = 0;
            }
            match sentences.recv() {
                Ok(batch) => queues.add(batch),
                Err(_) => return,
            }
        }
        sentences.try_iter().for_each(|batch| queues.add(batch));
        let resting = rest_until.is_some_and(|until| Instant::now() < until) && queues.soon.is_empty();
        if hold.load(Relaxed) || resting {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let Some((priority, sentence)) = queues.next() else { continue };
        let start = Instant::now();
        if let Err(error) = tts.render(&sentence, Lang::English) {
            warn!(%error, sentence, "could not render ahead");
        }
        let took = start.elapsed();
        if took > SYNTHESIZED {
            rendered += 1;
            rest_until = (priority == Priority::Idle).then(|| Instant::now() + took);
        }
    }
}

#[derive(Default)]
struct Queues {
    soon: VecDeque<String>,
    idle: VecDeque<String>,
}

impl Queues {
    fn add(&mut self, (priority, sentences): (Priority, Vec<String>)) {
        match priority {
            Priority::Soon => self.soon.extend(sentences),
            Priority::Idle => self.idle.extend(sentences),
        }
    }

    fn is_empty(&self) -> bool {
        self.soon.is_empty() && self.idle.is_empty()
    }

    fn next(&mut self) -> Option<(Priority, String)> {
        match self.soon.pop_front() {
            Some(sentence) => Some((Priority::Soon, sentence)),
            None => self.idle.pop_front().map(|sentence| (Priority::Idle, sentence)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what it renders.
    struct Recording(Arc<Mutex<Vec<String>>>);

    impl Tts for Recording {
        fn speak(&mut self, text: &str, _lang: Lang, _sink: &mut dyn FnMut(&[i16])) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(text.to_owned());
            Ok(())
        }
    }

    #[test]
    fn renders_what_is_wanted_soon_first_and_waits_while_held() {
        let done = Arc::new(Mutex::new(Vec::new()));
        let render = RenderAhead::spawn(Box::new(Recording(done.clone()))).unwrap();
        render.hold(true);
        render.add(Priority::Idle, vec!["It's 03:00.".into(), "It's 03:01.".into()]);
        render.add(Priority::Soon, vec!["Very good, sir.".into()]);
        std::thread::sleep(Duration::from_millis(300));
        assert!(done.lock().unwrap().is_empty());
        render.hold(false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while done.lock().unwrap().len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(*done.lock().unwrap(), ["Very good, sir.", "It's 03:00.", "It's 03:01."]);
    }
}

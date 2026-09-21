use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::Thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rtrb::{Consumer, Producer, RingBuffer};
use tokio::sync::Notify;
use tracing::{info, warn};
use voice_codec::packet::PacketPcm;
use voice_codec::{PacketDecoder, PacketEncoder};
use voice_proto::{Codec, Header, Kind, FRAME_SAMPLES, HEADER_LEN, MAX_DATAGRAM, PACKET_SAMPLES};
use voice_rt::jitter::{JitterBuffer, Popped, Pushed};
use voice_rt::noalloc::no_alloc;
use voice_rt::threads::spawn_pinned;

use crate::engine::FrameProcessor;
use crate::metrics::Metrics;
use crate::transport::Transport;

/// Frames of slack in each ring between the network task and the model thread.
const RING_FRAMES: usize = 16;

#[derive(Debug, Clone)]
pub struct Config {
    /// Reordering tolerance in 20 ms packets. See `JitterBuffer`.
    pub jitter_depth: u32,
    pub model_core: Option<usize>,
    pub session_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self { jitter_depth: 2, model_core: None, session_timeout: Duration::from_secs(5) }
    }
}

struct Peer {
    addr: SocketAddr,
    session: u16,
    last_seen: Instant,
    /// Bye received: input is closed, the address is kept so the tail of the output still goes out.
    closing: bool,
}

/// Network-side state. One session at a time; multi-session scheduling is out of scope for M1.
struct Net {
    cfg: Config,
    metrics: Arc<Metrics>,
    jitter: JitterBuffer,
    decoder: PacketDecoder,
    encoder: PacketEncoder,
    peer: Option<Peer>,
    to_model: Producer<i16>,
    from_model: Consumer<i16>,
    marks_to_model: Producer<Instant>,
    marks_from_model: Consumer<Instant>,
    generation: Arc<AtomicU64>,
    /// Last generation the model thread has reset for. Audio is refused until it catches up, so
    /// the discard on the model side can never eat samples this side has already counted.
    acked: Arc<AtomicU64>,
    model_thread: Thread,
    in_samples: u64,
    out_samples: u64,
    out_seq: u32,
    pcm: PacketPcm,
}

impl Net {
    /// Synchronous and allocation-free. Returns a header to send back when the datagram was a
    /// Hello that opened a session.
    fn handle_datagram(&mut self, data: &[u8], from: SocketAddr, now: Instant) -> Option<Header> {
        self.metrics.packets_rx.fetch_add(1, Relaxed);
        let Ok((header, payload)) = Header::parse(data) else {
            self.metrics.rejected.fetch_add(1, Relaxed);
            return None;
        };
        match header.kind {
            Kind::Hello => {
                let free = self.peer.as_ref().is_none_or(|p| {
                    p.addr == from || p.closing || now.duration_since(p.last_seen) > self.cfg.session_timeout
                });
                let (Ok(decoder), Ok(encoder)) = (PacketDecoder::new(header.codec), PacketEncoder::new(header.codec))
                else {
                    self.metrics.rejected.fetch_add(1, Relaxed);
                    return None;
                };
                if !free {
                    self.metrics.rejected.fetch_add(1, Relaxed);
                    return None;
                }
                self.decoder = decoder;
                self.encoder = encoder;
                self.open_session(from, header.session, now);
                Some(Header { kind: Kind::Hello, ..header })
            }
            Kind::Audio | Kind::Bye => {
                let Some(peer) = self.peer.as_mut().filter(|p| p.addr == from && p.session == header.session) else {
                    self.metrics.rejected.fetch_add(1, Relaxed);
                    return None;
                };
                let model_ready = self.acked.load(Relaxed) == self.generation.load(Relaxed);
                if peer.closing || !model_ready || header.codec != self.decoder.codec() {
                    self.metrics.rejected.fetch_add(1, Relaxed);
                    return None;
                }
                peer.last_seen = now;
                if header.kind == Kind::Bye {
                    peer.closing = true;
                    self.drain_jitter(now, true);
                    // Pad to a frame boundary so the model thread processes the tail.
                    while !self.in_samples.is_multiple_of(FRAME_SAMPLES as u64) {
                        self.pcm.fill(0);
                        if !self.push_pcm(now) {
                            break;
                        }
                    }
                    return None;
                }
                match self.jitter.push(header.seq, payload, now) {
                    Pushed::Accepted => {}
                    Pushed::Late => drop(self.metrics.late.fetch_add(1, Relaxed)),
                    Pushed::Duplicate => drop(self.metrics.duplicate.fetch_add(1, Relaxed)),
                    Pushed::Resync => drop(self.metrics.resync.fetch_add(1, Relaxed)),
                }
                self.drain_jitter(now, false);
                None
            }
        }
    }

    fn open_session(&mut self, addr: SocketAddr, session: u16, now: Instant) {
        self.peer = Some(Peer { addr, session, last_seen: now, closing: false });
        self.jitter.reset_at(0);
        // The model thread discards its pending input and resets streaming state when it sees
        // the new generation.
        self.generation.fetch_add(1, Relaxed);
        self.model_thread.unpark();
        discard(&mut self.from_model);
        while self.marks_from_model.pop().is_ok() {}
        self.in_samples = 0;
        self.out_samples = 0;
        self.out_seq = 0;
        self.metrics.sessions.fetch_add(1, Relaxed);
    }

    fn drain_jitter(&mut self, now: Instant, flush: bool) {
        loop {
            let popped = if flush { self.jitter.pop_flush(now) } else { self.jitter.pop(now) };
            match popped {
                None => break,
                Some(Popped::Packet { payload, waited, .. }) => {
                    self.metrics.jitter_wait.record(waited);
                    if self.decoder.decode(payload, &mut self.pcm).is_err() {
                        self.metrics.rejected.fetch_add(1, Relaxed);
                        self.decoder.conceal(&mut self.pcm);
                    }
                }
                Some(Popped::Missing { .. }) => {
                    self.metrics.lost.fetch_add(1, Relaxed);
                    self.decoder.conceal(&mut self.pcm);
                }
            }
            self.push_pcm(now);
        }
    }

    /// Hands `self.pcm` to the model thread. Returns false when the ring is full.
    fn push_pcm(&mut self, now: Instant) -> bool {
        if self.to_model.push_entire_slice(&self.pcm).is_err() {
            self.metrics.overruns.fetch_add(1, Relaxed);
            return false;
        }
        self.in_samples += PACKET_SAMPLES as u64;
        if self.in_samples.is_multiple_of(FRAME_SAMPLES as u64) {
            let _ = self.marks_to_model.push(now);
            self.model_thread.unpark();
        }
        true
    }

    async fn send_output<T: Transport>(&mut self, transport: &mut T, out: &mut [u8; MAX_DATAGRAM]) {
        while self.from_model.slots() >= PACKET_SAMPLES {
            let Some(peer) = &self.peer else {
                discard(&mut self.from_model);
                return;
            };
            let (addr, session) = (peer.addr, peer.session);
            if self.from_model.pop_entire_slice(&mut self.pcm).is_err() {
                return;
            }
            let header = Header {
                kind: Kind::Audio,
                codec: self.decoder.codec(),
                session,
                seq: self.out_seq,
                ts_samples: self.out_samples as u32,
            };
            header.write(out);
            let Ok(len) = self.encoder.encode(&self.pcm, &mut out[HEADER_LEN..]) else { return };
            if let Err(error) = transport.send(&out[..HEADER_LEN + len], addr).await {
                warn!(%error, "send failed");
            }
            self.metrics.packets_tx.fetch_add(1, Relaxed);
            if self.out_samples.is_multiple_of(FRAME_SAMPLES as u64) {
                if let Ok(mark) = self.marks_from_model.pop() {
                    self.metrics.frame_total.record(mark.elapsed());
                }
            }
            self.out_seq = self.out_seq.wrapping_add(1);
            self.out_samples += PACKET_SAMPLES as u64;
        }
    }

    fn expire_session(&mut self, now: Instant) {
        if self.peer.as_ref().is_some_and(|p| now.duration_since(p.last_seen) > self.cfg.session_timeout) {
            info!("session expired");
            self.peer = None;
        }
    }
}

fn discard<T>(consumer: &mut Consumer<T>) {
    if let Ok(chunk) = consumer.read_chunk(consumer.slots()) {
        chunk.commit_all();
    }
}

struct ModelSide {
    input: Consumer<i16>,
    output: Producer<i16>,
    marks_in: Consumer<Instant>,
    marks_out: Producer<Instant>,
    generation: Arc<AtomicU64>,
    acked: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    output_ready: Arc<Notify>,
    metrics: Arc<Metrics>,
}

fn model_loop(mut side: ModelSide, mut processor: Box<dyn FrameProcessor>) {
    let mut frame_in = [0i16; FRAME_SAMPLES];
    let mut frame_out = [0i16; FRAME_SAMPLES];
    let mut generation = side.generation.load(Relaxed);
    while !side.stop.load(Relaxed) {
        let current = side.generation.load(Relaxed);
        if current != generation {
            generation = current;
            discard(&mut side.input);
            discard(&mut side.marks_in);
            processor.reset();
            side.acked.store(generation, Relaxed);
        }
        if side.input.slots() < FRAME_SAMPLES {
            std::thread::park_timeout(Duration::from_millis(5));
            continue;
        }
        let produced = no_alloc(|| {
            if side.input.pop_entire_slice(&mut frame_in).is_err() {
                return false;
            }
            let start = Instant::now();
            if processor.process(&frame_in, &mut frame_out, &side.metrics).is_err() {
                side.metrics.process_errors.fetch_add(1, Relaxed);
                frame_out.fill(0);
            }
            let elapsed = start.elapsed();
            side.metrics.frame_process.record(elapsed);
            side.metrics.last_process_us.store(elapsed.as_micros() as u64, Relaxed);
            side.metrics.frames.fetch_add(1, Relaxed);

            let mark = side.marks_in.pop().ok();
            if side.output.push_entire_slice(&frame_out).is_err() {
                side.metrics.overruns.fetch_add(1, Relaxed);
                return false;
            }
            if let Some(mark) = mark {
                let _ = side.marks_out.push(mark);
            }
            true
        });
        // Outside the guard: waking the network task goes through tokio's inject queue, which
        // takes a mutex (and on macOS allocates it on first use).
        if produced {
            side.output_ready.notify_one();
        }
    }
}

enum Event {
    Datagram(std::io::Result<(usize, SocketAddr)>),
    OutputReady,
    Tick,
    Shutdown,
}

/// Runs the server until `shutdown` resolves.
pub async fn serve<T: Transport>(
    mut transport: T,
    processor: Box<dyn FrameProcessor>,
    cfg: Config,
    metrics: Arc<Metrics>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let ring = FRAME_SAMPLES * RING_FRAMES;
    let (to_model, input) = RingBuffer::new(ring);
    let (output, from_model) = RingBuffer::new(ring);
    let (marks_to_model, marks_in) = RingBuffer::new(RING_FRAMES * 2);
    let (marks_out, marks_from_model) = RingBuffer::new(RING_FRAMES * 2);
    let generation = Arc::new(AtomicU64::new(0));
    let acked = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let output_ready = Arc::new(Notify::new());

    let side = ModelSide {
        input,
        output,
        marks_in,
        marks_out,
        generation: generation.clone(),
        acked: acked.clone(),
        stop: stop.clone(),
        output_ready: output_ready.clone(),
        metrics: metrics.clone(),
    };
    let model = spawn_pinned("model", cfg.model_core, move |pinned| {
        info!(pinned, "model thread started");
        model_loop(side, processor);
    })
    .context("spawning model thread")?;

    let mut net = Net {
        jitter: JitterBuffer::new(cfg.jitter_depth),
        decoder: PacketDecoder::new(Codec::PcmS16)?,
        encoder: PacketEncoder::new(Codec::PcmS16)?,
        peer: None,
        to_model,
        from_model,
        marks_to_model,
        marks_from_model,
        generation,
        acked,
        model_thread: model.thread().clone(),
        in_samples: 0,
        out_samples: 0,
        out_seq: 0,
        pcm: [0; PACKET_SAMPLES],
        cfg,
        metrics,
    };

    // One byte larger than the protocol allows, so oversized datagrams are detected, not truncated.
    let mut rx = [0u8; MAX_DATAGRAM + 1];
    let mut tx = [0u8; MAX_DATAGRAM];
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tokio::pin!(shutdown);

    loop {
        let event = tokio::select! {
            received = transport.recv(&mut rx) => Event::Datagram(received),
            () = output_ready.notified() => Event::OutputReady,
            _ = tick.tick() => Event::Tick,
            () = &mut shutdown => Event::Shutdown,
        };
        match event {
            Event::Datagram(Ok((len, from))) => {
                let reply = no_alloc(|| net.handle_datagram(&rx[..len], from, Instant::now()));
                if let Some(header) = reply {
                    info!(%from, session = header.session, codec = ?header.codec, "session opened");
                    header.write(&mut tx);
                    if let Err(error) = transport.send(&tx[..HEADER_LEN], from).await {
                        warn!(%error, "hello reply failed");
                    }
                }
            }
            Event::Datagram(Err(error)) => warn!(%error, "recv failed"),
            Event::OutputReady => net.send_output(&mut transport, &mut tx).await,
            Event::Tick => net.expire_session(Instant::now()),
            Event::Shutdown => break,
        }
    }

    stop.store(true, Relaxed);
    model.thread().unpark();
    let _ = model.join();
    Ok(())
}

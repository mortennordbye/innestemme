//! Test client for the voice engine: streams a tone, a wav file or the microphone, and reports
//! how long the server takes to turn each 80 ms frame around.

mod resample;

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use resample::{Fir2, DELAY_FRAMES};
use voice_proto::{
    pcm_from_bytes, pcm_to_bytes, Codec, Header, Kind, HEADER_LEN, MAX_DATAGRAM, PACKETS_PER_FRAME, PACKET_SAMPLES,
    SAMPLE_RATE,
};
use voice_rt::hist::Histogram;

const PACKET_PERIOD: Duration = Duration::from_millis(20);
const HELLO_RETRY: Duration = Duration::from_millis(500);
/// Longer than the server's default session timeout (5 s): a previous client that died without a
/// Bye holds the session until then, and its Hellos are refused in the meantime.
const HELLO_WINDOW: Duration = Duration::from_secs(7);

#[derive(Parser)]
#[command(about = "Voice engine test client")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:7000")]
    server: SocketAddr,
    /// 24 kHz mono 16-bit wav to stream instead of the microphone.
    #[arg(long, conflicts_with = "tone")]
    wav_in: Option<PathBuf>,
    /// Stream a 440 Hz tone for this many seconds instead of the microphone.
    #[arg(long)]
    tone: Option<f32>,
    /// Stream this many 1 kHz bursts, about one per second, and report the mouth-to-ear delay.
    #[arg(long, conflicts_with_all = ["tone", "wav_in"])]
    marker: Option<usize>,
    /// Write the returned audio here (file modes only).
    #[arg(long)]
    wav_out: Option<PathBuf>,
    /// Live mode duration in seconds.
    #[arg(long, default_value_t = 10.0)]
    seconds: f32,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect(args.server)?;
    let session = (std::process::id() & 0xffff) as u16;
    hello(&socket, session)?;

    let (source, onsets) = match (&args.wav_in, args.tone, args.marker) {
        (Some(path), ..) => (Some(read_wav(path)?), Vec::new()),
        (None, Some(seconds), _) => (Some(tone(seconds)), Vec::new()),
        (None, None, Some(bursts)) => {
            let (pcm, onsets) = marker(bursts);
            (Some(pcm), onsets)
        }
        (None, None, None) => (None, Vec::new()),
    };
    match source {
        Some(pcm) => run_file(&socket, session, &pcm, &onsets, args.wav_out.as_deref()),
        None => run_live(&socket, session, args.seconds),
    }
}

fn header(kind: Kind, session: u16, seq: u32) -> Header {
    Header { kind, codec: Codec::PcmS16, session, seq, ts_samples: seq.wrapping_mul(PACKET_SAMPLES as u32) }
}

fn hello(socket: &UdpSocket, session: u16) -> Result<()> {
    let mut buf = [0u8; MAX_DATAGRAM];
    header(Kind::Hello, session, 0).write(&mut buf);
    let mut rx = [0u8; MAX_DATAGRAM];
    socket.set_read_timeout(Some(HELLO_RETRY))?;
    let deadline = Instant::now() + HELLO_WINDOW;
    while Instant::now() < deadline {
        let attempt = Instant::now();
        // A connected UDP socket reports ICMP port-unreachable as an error on the next send or
        // recv, without waiting for the read timeout. That only means the server is not bound yet.
        let reply = socket.send(&buf[..HEADER_LEN]).and_then(|_| socket.recv(&mut rx));
        if let Ok(n) = reply {
            if matches!(Header::parse(&rx[..n]), Ok((h, _)) if h.kind == Kind::Hello && h.session == session) {
                // The server refuses audio until its model thread has reset for the new session.
                std::thread::sleep(Duration::from_millis(20));
                return Ok(());
            }
        }
        std::thread::sleep(HELLO_RETRY.saturating_sub(attempt.elapsed()));
    }
    bail!("no hello reply from server within {HELLO_WINDOW:?}")
}

fn read_wav(path: &std::path::Path) -> Result<Vec<i16>> {
    let mut reader = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != SAMPLE_RATE || spec.channels != 1 || spec.bits_per_sample != 16 {
        bail!("{} must be 24 kHz mono 16-bit, got {spec:?}", path.display());
    }
    Ok(reader.samples::<i16>().collect::<Result<_, _>>()?)
}

fn tone(seconds: f32) -> Vec<i16> {
    let n = (seconds * SAMPLE_RATE as f32) as usize;
    (0..n).map(|i| ((i as f32 / SAMPLE_RATE as f32 * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16).collect()
}

/// Silence with a 100 ms 1 kHz burst roughly every second. Returns the signal and the sample index
/// where each burst starts. The spacing is not a multiple of the 80 ms frame, so the bursts land
/// at different positions within a frame.
fn marker(bursts: usize) -> (Vec<i16>, Vec<usize>) {
    let rate = SAMPLE_RATE as usize;
    let spacing = rate + 431;
    let onsets: Vec<usize> = (0..bursts).map(|b| rate + b * spacing).collect();
    let mut pcm = vec![0i16; rate * 2 + bursts * spacing];
    for &onset in &onsets {
        for (i, sample) in pcm[onset..onset + rate / 10].iter_mut().enumerate() {
            *sample = ((i as f32 / SAMPLE_RATE as f32 * 1000.0 * std::f32::consts::TAU).sin() * 12000.0) as i16;
        }
    }
    (pcm, onsets)
}

/// Where a burst sent at `onset` starts in the returned audio: the first sample above a quarter of
/// the local peak. `None` when nothing came back around it.
fn find_onset(out: &[i16], onset: usize) -> Option<usize> {
    let rate = SAMPLE_RATE as usize;
    let start = onset.saturating_sub(rate / 10).min(out.len());
    let window = &out[start..(onset + rate).min(out.len())];
    let peak = window.iter().map(|s| s.unsigned_abs()).max().filter(|&peak| peak >= 1000)?;
    Some(start + window.iter().position(|s| s.unsigned_abs() > peak / 4)?)
}

/// Receives until the socket is quiet, recording when each output packet arrived.
fn receive(socket: &UdpSocket, stop: &AtomicBool, mut on_packet: impl FnMut(u32, &[i16; PACKET_SAMPLES], Instant)) {
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut pcm = [0i16; PACKET_SAMPLES];
    let mut quiet_since = Instant::now();
    loop {
        match socket.recv(&mut buf) {
            Ok(n) => {
                let now = Instant::now();
                quiet_since = now;
                let Ok((h, payload)) = Header::parse(&buf[..n]) else {
                    continue;
                };
                if h.kind == Kind::Audio && pcm_from_bytes(payload, &mut pcm) == PACKET_SAMPLES {
                    on_packet(h.seq, &pcm, now);
                }
            }
            Err(_) if stop.load(Relaxed) && quiet_since.elapsed() > Duration::from_millis(600) => return,
            Err(_) => {}
        }
    }
}

fn run_file(
    socket: &UdpSocket,
    session: u16,
    pcm: &[i16],
    onsets: &[usize],
    wav_out: Option<&std::path::Path>,
) -> Result<()> {
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let packets = pcm.as_chunks::<PACKET_SAMPLES>().0;
    let sent_at: Arc<Vec<AtomicU64>> = Arc::new((0..packets.len()).map(|_| AtomicU64::new(0)).collect());
    let epoch = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let received = Arc::new(Mutex::new(Vec::<(u32, [i16; PACKET_SAMPLES], Instant)>::new()));

    let receiver = {
        let (socket, stop, received) = (socket.try_clone()?, stop.clone(), received.clone());
        std::thread::spawn(move || {
            receive(&socket, &stop, |seq, pcm, at| received.lock().unwrap().push((seq, *pcm, at)))
        })
    };

    let mut buf = [0u8; MAX_DATAGRAM];
    for (seq, packet) in packets.iter().enumerate() {
        header(Kind::Audio, session, seq as u32).write(&mut buf);
        let len = pcm_to_bytes(packet, &mut buf[HEADER_LEN..]);
        sent_at[seq].store(epoch.elapsed().as_micros() as u64, Relaxed);
        socket.send(&buf[..HEADER_LEN + len])?;
        let next = epoch + PACKET_PERIOD * (seq as u32 + 1);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    header(Kind::Bye, session, packets.len() as u32).write(&mut buf);
    socket.send(&buf[..HEADER_LEN])?;
    stop.store(true, Relaxed);
    receiver.join().unwrap();

    let mut received = Arc::try_unwrap(received).ok().context("receiver still running")?.into_inner().unwrap();
    received.sort_by_key(|(seq, ..)| *seq);

    // Output packet k carries the processed input packet k. The first packet of a frame can only
    // leave the server once the last input packet of that frame has arrived, so the turnaround
    // is measured from that send.
    let turnaround = Histogram::new();
    for (seq, _, at) in &received {
        let completes = *seq as usize + PACKETS_PER_FRAME - 1;
        if (*seq as usize).is_multiple_of(PACKETS_PER_FRAME) && completes < sent_at.len() {
            let sent = epoch + Duration::from_micros(sent_at[completes].load(Relaxed));
            turnaround.record(at.saturating_duration_since(sent));
        }
    }
    println!("sent {} packets, received {}", packets.len(), received.len());
    println!(
        "frame turnaround (network + server, excludes the 80 ms frame fill): mean {:?}, p50 <= {:?}, p99 <= {:?}",
        turnaround.mean().unwrap_or_default(),
        turnaround.quantile_upper_bound(0.5).unwrap_or_default(),
        turnaround.quantile_upper_bound(0.99).unwrap_or_default(),
    );

    if !onsets.is_empty() {
        report_mouth_to_ear(&received, epoch, onsets);
    }

    if let Some(path) = wav_out {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec)?;
        for (_, pcm, _) in &received {
            pcm.iter().try_for_each(|&s| writer.write_sample(s))?;
        }
        writer.finalize()?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// Mouth-to-ear delay without the audio devices, split into its two parts.
///
/// Stream delay: input sample `s` is captured at `epoch - 20 ms + s / rate` (a packet leaves when
/// its last sample exists). Gapless playback plays output sample `s` at `t0 + s / rate`, and the
/// earliest `t0` that never finds a packet missing is the largest `arrival - position` seen. The
/// p99 is printed next to it because a single slow frame sets the worst case.
/// Content shift: how much later the burst sits in the output stream than in the input stream,
/// which is the delay inside the codec.
fn report_mouth_to_ear(received: &[(u32, [i16; PACKET_SAMPLES], Instant)], epoch: Instant, onsets: &[usize]) {
    let rate = f64::from(SAMPLE_RATE);
    let position = |seq: u32| f64::from(seq) * PACKET_SAMPLES as f64 / rate;
    let mut delays_ms: Vec<f64> = received
        .iter()
        .map(|(seq, _, at)| at.saturating_duration_since(epoch).as_secs_f64() - position(*seq))
        .map(|t0| (t0 + PACKET_PERIOD.as_secs_f64()) * 1e3)
        .collect();
    delays_ms.sort_unstable_by(f64::total_cmp);
    let Some(&stream_ms) = delays_ms.last() else {
        println!("mouth-to-ear: nothing came back");
        return;
    };
    let p99_ms = delays_ms[(delays_ms.len() * 99).div_ceil(100) - 1];

    let len = received.last().map_or(0, |(seq, ..)| (*seq as usize + 1) * PACKET_SAMPLES);
    let mut out = vec![0i16; len];
    for (seq, pcm, _) in received {
        let at = *seq as usize * PACKET_SAMPLES;
        out[at..at + PACKET_SAMPLES].copy_from_slice(pcm);
    }
    let shifts_ms: Vec<f64> = onsets
        .iter()
        .filter_map(|&onset| Some((find_onset(&out, onset)? as f64 - onset as f64) / rate * 1e3))
        .collect();
    if shifts_ms.is_empty() {
        println!("mouth-to-ear: no marker burst came back");
        return;
    }
    let mean = shifts_ms.iter().sum::<f64>() / shifts_ms.len() as f64;
    let min = shifts_ms.iter().copied().fold(f64::INFINITY, f64::min);
    let max = shifts_ms.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    println!("marker bursts found: {} of {}", shifts_ms.len(), onsets.len());
    println!(
        "stream delay (80 ms frame fill + server + network, no playout margin): {stream_ms:.1} ms worst packet, \
         {p99_ms:.1} ms p99"
    );
    println!("content shift inside the codec: mean {mean:.1} ms (min {min:.1}, max {max:.1})");
    println!("mouth-to-ear without audio devices: {:.1} ms", stream_ms + mean);
}

/// Device rate handling: 24 kHz natively, or 48 kHz through the 2:1 FIR.
fn pick_config(device: &cpal::Device, input: bool) -> Result<(cpal::StreamConfig, bool)> {
    let default = if input { device.default_input_config()? } else { device.default_output_config()? };
    if default.sample_format() != cpal::SampleFormat::F32 {
        bail!("only f32 audio devices are supported, got {:?}", default.sample_format());
    }
    let mut config: cpal::StreamConfig = default.into();
    let double = match config.sample_rate {
        r if r == SAMPLE_RATE => false,
        r if r == SAMPLE_RATE * 2 => true,
        r => bail!("device runs at {r} Hz; only 24 and 48 kHz are supported"),
    };
    config.buffer_size = cpal::BufferSize::Default;
    Ok((config, double))
}

fn run_live(socket: &UdpSocket, session: u16, seconds: f32) -> Result<()> {
    let host = cpal::default_host();
    let mic = host.default_input_device().context("no input device")?;
    let speaker = host.default_output_device().context("no output device")?;
    let (mic_cfg, mic_double) = pick_config(&mic, true)?;
    let (spk_cfg, spk_double) = pick_config(&speaker, false)?;

    let (mut captured_tx, mut captured_rx) = rtrb::RingBuffer::<i16>::new(SAMPLE_RATE as usize);
    let (mut playback_tx, mut playback_rx) = rtrb::RingBuffer::<i16>::new(SAMPLE_RATE as usize);
    // When the first sample of each packet was captured, and when the first sample of each
    // received packet reaches the speaker, both on the audio host clock.
    let packets = (seconds * 1000.0 / PACKET_PERIOD.as_millis() as f32) as usize + 100;
    let (mut captured_at_tx, mut captured_at_rx) = rtrb::RingBuffer::<cpal::StreamInstant>::new(packets);
    let (mut played_at_tx, mut played_at_rx) = rtrb::RingBuffer::<cpal::StreamInstant>::new(packets);
    let dropped = Arc::new(AtomicU64::new(0));
    let starved = Arc::new(AtomicU64::new(0));
    // Latest driver-reported latency per direction, in microseconds.
    let mic_latency = Arc::new(AtomicU64::new(0));
    let spk_latency = Arc::new(AtomicU64::new(0));

    let channels = mic_cfg.channels as usize;
    let frame = Duration::from_secs(1) / mic_cfg.sample_rate;
    let mut fir = Fir2::new();
    let mut captured = 0u64;
    let input = {
        let (dropped, mic_latency) = (dropped.clone(), mic_latency.clone());
        mic.build_input_stream(
            mic_cfg,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                let start = info.timestamp().capture;
                mic_latency.store((info.timestamp().callback - start).as_micros() as u64, Relaxed);
                for (i, frame_data) in data.chunks(channels).enumerate() {
                    let sample = if mic_double { fir.decimate(frame_data[0]) } else { Some(frame_data[0]) };
                    let Some(s) = sample else { continue };
                    if captured_tx.push((s * 32768.0).clamp(-32768.0, 32767.0) as i16).is_err() {
                        dropped.fetch_add(1, Relaxed);
                        continue;
                    }
                    if captured.is_multiple_of(PACKET_SAMPLES as u64) {
                        let _ = captured_at_tx.push(start + frame * i as u32);
                    }
                    captured += 1;
                }
            },
            |e| eprintln!("input stream error: {e}"),
            None,
        )?
    };

    let channels = spk_cfg.channels as usize;
    let frame = Duration::from_secs(1) / spk_cfg.sample_rate;
    let mut fir = Fir2::new();
    let mut pending: Option<f32> = None;
    let mut played = 0u64;
    // Silence since the last real sample. Only counted as starvation once audio resumes, so the
    // quiet tail after the stream ends is not.
    let mut gap = 0u64;
    let output = {
        let (starved, spk_latency) = (starved.clone(), spk_latency.clone());
        speaker.build_output_stream(
            spk_cfg,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                let start = info.timestamp().playback;
                spk_latency.store((start - info.timestamp().callback).as_micros() as u64, Relaxed);
                for (i, frame_data) in data.chunks_mut(channels).enumerate() {
                    let sample = match pending.take() {
                        Some(s) => s,
                        None => {
                            let x = match playback_rx.pop() {
                                Ok(s) => {
                                    if played.is_multiple_of(PACKET_SAMPLES as u64) {
                                        let _ = played_at_tx.push(start + frame * i as u32);
                                    }
                                    played += 1;
                                    if played > 1 {
                                        starved.fetch_add(gap, Relaxed);
                                    }
                                    gap = 0;
                                    f32::from(s) / 32768.0
                                }
                                Err(_) => {
                                    gap += 1;
                                    0.0
                                }
                            };
                            if spk_double {
                                let [a, b] = fir.interpolate(x);
                                pending = Some(b);
                                a
                            } else {
                                x
                            }
                        }
                    };
                    frame_data.fill(sample);
                }
            },
            |e| eprintln!("output stream error: {e}"),
            None,
        )?
    };
    input.play()?;
    output.play()?;
    println!("Microphone is live for {seconds:.0} s. Talk now. (Ctrl-C stops)");

    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let stop = Arc::new(AtomicBool::new(false));
    let receiver = {
        let (socket, stop) = (socket.try_clone()?, stop.clone());
        std::thread::spawn(move || {
            // Packets are queued whole or not at all, so the n-th packet played is `queued[n]`.
            let mut queued = Vec::<u32>::new();
            receive(&socket, &stop, |seq, pcm, _| {
                if playback_tx.push_entire_slice(pcm).is_ok() {
                    queued.push(seq);
                }
            });
            queued
        })
    };

    let mut buf = [0u8; MAX_DATAGRAM];
    let mut pcm = [0i16; PACKET_SAMPLES];
    let mut seq = 0u32;
    let deadline = Instant::now() + Duration::from_secs_f32(seconds);
    while Instant::now() < deadline {
        if captured_rx.pop_entire_slice(&mut pcm).is_err() {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        header(Kind::Audio, session, seq).write(&mut buf);
        let len = pcm_to_bytes(&pcm, &mut buf[HEADER_LEN..]);
        socket.send(&buf[..HEADER_LEN + len])?;
        seq = seq.wrapping_add(1);
    }
    header(Kind::Bye, session, seq).write(&mut buf);
    socket.send(&buf[..HEADER_LEN])?;
    stop.store(true, Relaxed);
    let queued = receiver.join().unwrap();
    // Let the queued tail play out so every received packet gets a playback time.
    std::thread::sleep(Duration::from_millis(300));
    drop((input, output));

    // The microphone keeps running after the last send; the server pads the last frame with packets
    // that carry no captured audio, so only sent packets are timed.
    let captured_at: Vec<_> = std::iter::from_fn(|| captured_at_rx.pop().ok()).take(seq as usize).collect();
    let played_at: Vec<_> = std::iter::from_fn(|| played_at_rx.pop().ok()).collect();
    println!("sent {seq} packets, received and queued {}", queued.len());
    if dropped.load(Relaxed) > 0 {
        println!("capture ring overflowed ({} samples); delays below are not trustworthy", dropped.load(Relaxed));
    }
    let resampler = Duration::from_secs(1) / (SAMPLE_RATE * 2)
        * DELAY_FRAMES as u32
        * (u32::from(mic_double) + u32::from(spk_double));
    println!(
        "driver-reported device latency: microphone {:.1} ms, speaker {:.1} ms",
        mic_latency.load(Relaxed) as f64 / 1e3,
        spk_latency.load(Relaxed) as f64 / 1e3
    );
    report_live_mouth_to_ear(&captured_at, &played_at, &queued, resampler, starved.load(Relaxed));
    Ok(())
}

/// Mouth-to-ear with the audio devices: from the moment the microphone captured the first sample
/// of a packet to the moment the speaker plays the returned copy of it. cpal's timestamps include
/// the device buffers and the latency the driver reports. Any shift inside the codec is not
/// included (the marker mode measures it: 0 ms for Mimi).
fn report_live_mouth_to_ear(
    captured_at: &[cpal::StreamInstant],
    played_at: &[cpal::StreamInstant],
    queued: &[u32],
    resampler: Duration,
    starved: u64,
) {
    let mut delays_ms: Vec<f64> = played_at
        .iter()
        .zip(queued)
        .filter_map(|(played, &seq)| played.checked_duration_since(*captured_at.get(seq as usize)?))
        .map(|d| (d + resampler).as_secs_f64() * 1e3)
        .collect();
    if delays_ms.is_empty() {
        println!("mouth-to-ear: nothing was played back");
        return;
    }
    delays_ms.sort_unstable_by(f64::total_cmp);
    let at = |q: usize| delays_ms[(delays_ms.len() * q).div_ceil(100).max(1) - 1];
    println!(
        "mouth-to-ear with audio devices ({} packets): p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
        delays_ms.len(),
        at(50),
        at(99),
        at(100)
    );
    let underruns_ms = starved as f64 / f64::from(SAMPLE_RATE) * 1e3;
    println!("speaker ran dry for {underruns_ms:.0} ms mid-stream (each gap adds to the delay of everything after it)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_onsets_are_found_after_a_shift_and_a_gain_change() {
        let (pcm, onsets) = marker(3);
        assert_eq!(onsets.len(), 3);
        assert!(onsets.iter().any(|onset| onset % voice_proto::FRAME_SAMPLES != 0));

        let shift = 1234;
        let mut out = vec![0i16; shift];
        out.extend(pcm.iter().map(|&s| s / 3));
        for &onset in &onsets {
            let found = find_onset(&out, onset).expect("burst");
            // A 1 kHz sine crosses a quarter of its peak within the first few samples.
            assert!((shift..shift + 4).contains(&(found - onset)), "found {found} for onset {onset}");
        }
        assert_eq!(find_onset(&vec![0i16; pcm.len()], onsets[0]), None);
    }
}

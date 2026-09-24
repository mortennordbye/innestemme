use std::collections::VecDeque;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use voice_rt::hist::Histogram;

/// Everything here is recorded with atomics only, so the audio path can update it freely.
#[derive(Default)]
pub struct Metrics {
    /// Arrival to release from the reorder buffer.
    pub jitter_wait: Histogram,
    pub mimi_encode: Histogram,
    pub engine_step: Histogram,
    pub mimi_decode: Histogram,
    /// Whole `FrameProcessor::process` call.
    pub frame_process: Histogram,
    /// Input frame complete to first output packet handed to the socket.
    pub frame_total: Histogram,

    pub packets_rx: AtomicU64,
    pub packets_tx: AtomicU64,
    pub rejected: AtomicU64,
    pub late: AtomicU64,
    pub duplicate: AtomicU64,
    pub lost: AtomicU64,
    pub resync: AtomicU64,
    pub frames: AtomicU64,
    /// Frames or packets dropped because a ring between the threads was full.
    pub overruns: AtomicU64,
    pub process_errors: AtomicU64,
    pub sessions: AtomicU64,
    pub last_process_us: AtomicU64,
}

impl Metrics {
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(8192);
        let hists = [
            (&self.jitter_wait, "voice_jitter_wait_seconds", "Time a packet spent in the reorder buffer."),
            (&self.mimi_encode, "voice_mimi_encode_seconds", "Mimi encode step per 80 ms frame."),
            (&self.engine_step, "voice_engine_step_seconds", "Engine step per 80 ms frame."),
            (&self.mimi_decode, "voice_mimi_decode_seconds", "Mimi decode step per 80 ms frame."),
            (&self.frame_process, "voice_frame_process_seconds", "Model thread time per 80 ms frame."),
            (&self.frame_total, "voice_frame_total_seconds", "Input frame complete to first output packet sent."),
        ];
        for (hist, name, help) in hists {
            hist.render(name, help, &mut out);
        }
        let counters = [
            (&self.packets_rx, "voice_packets_received_total"),
            (&self.packets_tx, "voice_packets_sent_total"),
            (&self.rejected, "voice_packets_rejected_total"),
            (&self.late, "voice_packets_late_total"),
            (&self.duplicate, "voice_packets_duplicate_total"),
            (&self.lost, "voice_packets_lost_total"),
            (&self.resync, "voice_jitter_resync_total"),
            (&self.frames, "voice_frames_total"),
            (&self.overruns, "voice_ring_overruns_total"),
            (&self.process_errors, "voice_process_errors_total"),
            (&self.sessions, "voice_sessions_total"),
        ];
        for (counter, name) in counters {
            let _ = writeln!(out, "# TYPE {name} counter\n{name} {}", counter.load(Relaxed));
        }
        let last = self.last_process_us.load(Relaxed);
        let rtf = if last == 0 { 0.0 } else { 80_000.0 / last as f64 };
        let _ = writeln!(out, "# HELP voice_realtime_factor 80 ms divided by the last frame's processing time.");
        let _ = writeln!(out, "# TYPE voice_realtime_factor gauge\nvoice_realtime_factor {rtf}");
        out
    }
}

/// Answers as wav files for satellites that play a URL (a Voice PE's media player). The last few
/// are kept; a device fetches its answer within seconds. A clip can be fetched while it is still
/// being written: the response then streams until the clip is finished.
#[derive(Default)]
pub struct SpeechClips {
    clips: Mutex<VecDeque<(String, Arc<Clip>)>>,
    next: std::sync::atomic::AtomicU64,
}

const KEEP_CLIPS: usize = 8;
/// A streaming response gives up on a clip that stops growing for this long.
const STALL_TIMEOUT: Duration = Duration::from_secs(20);

/// One wav, possibly still growing.
pub struct Clip {
    state: Mutex<ClipState>,
    grew: Notify,
}

struct ClipState {
    wav: Vec<u8>,
    done: bool,
}

impl Clip {
    /// Appends mono 16-bit samples.
    pub fn push(&self, pcm: &[i16]) {
        self.state.lock().unwrap().wav.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
        self.grew.notify_waiters();
    }

    /// No more audio: streaming responses end, and the header gets the real length.
    pub fn finish(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.done {
            state.done = true;
            let data_len = (state.wav.len() - WAV_HEADER) as u32;
            state.wav[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
            state.wav[40..44].copy_from_slice(&data_len.to_le_bytes());
        }
        drop(state);
        self.grew.notify_waiters();
    }

    /// Bytes from `from` on, and whether the clip is complete.
    fn read(&self, from: usize) -> (Vec<u8>, bool) {
        let state = self.state.lock().unwrap();
        (state.wav.get(from..).unwrap_or_default().to_vec(), state.done)
    }
}

const WAV_HEADER: usize = 44;

/// A wav header; `u32::MAX` lengths while the length is not known yet, as streaming encoders write.
fn wav_header(rate: u32) -> Vec<u8> {
    let mut wav = Vec::with_capacity(WAV_HEADER);
    wav.extend_from_slice(b"RIFF");
    wav.extend(u32::MAX.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(rate.to_le_bytes());
    wav.extend((rate * 2).to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend(u32::MAX.to_le_bytes());
    wav
}

impl SpeechClips {
    /// Stores mono 16-bit PCM as a wav; the path to fetch it from.
    pub fn put(&self, pcm: &[i16], rate: u32) -> String {
        let (path, clip) = self.open(rate);
        clip.push(pcm);
        clip.finish();
        path
    }

    /// Starts a clip to be written as audio arrives; its path, fetchable at once.
    pub fn open(&self, rate: u32) -> (String, Arc<Clip>) {
        let n = self.next.fetch_add(1, Relaxed);
        let id = format!("{n:x}-{}", std::process::id());
        let clip =
            Arc::new(Clip { state: Mutex::new(ClipState { wav: wav_header(rate), done: false }), grew: Notify::new() });
        let mut clips = self.clips.lock().unwrap();
        clips.push_back((id.clone(), clip.clone()));
        while clips.len() > KEEP_CLIPS {
            if let Some((_, old)) = clips.pop_front() {
                old.finish();
            }
        }
        (format!("/speech/{id}.wav"), clip)
    }

    fn get(&self, id: &str) -> Option<Arc<Clip>> {
        self.clips.lock().unwrap().iter().find(|(k, _)| k == id).map(|(_, v)| v.clone())
    }
}

/// Writes a clip: whole with a length when it is complete, else chunked as it grows.
async fn send_clip(stream: &mut TcpStream, clip: &Clip) -> std::io::Result<()> {
    let (bytes, done) = clip.read(0);
    if done {
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: audio/wav\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            bytes.len()
        );
        stream.write_all(head.as_bytes()).await?;
        return stream.write_all(&bytes).await;
    }
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: audio/wav\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        )
        .await?;
    // The header as it was when streaming began: a finished clip's header is rewritten with the
    // real length, which this response can no longer change.
    let mut sent = 0;
    loop {
        let grew = clip.grew.notified();
        tokio::pin!(grew);
        grew.as_mut().enable();
        let (bytes, done) = clip.read(sent);
        let bytes = if sent == 0 && bytes.len() >= WAV_HEADER {
            let mut head = wav_header(u32::from_le_bytes(bytes[24..28].try_into().unwrap()));
            head.extend_from_slice(&bytes[WAV_HEADER..]);
            head
        } else {
            bytes
        };
        if !bytes.is_empty() {
            stream.write_all(format!("{:x}\r\n", bytes.len()).as_bytes()).await?;
            stream.write_all(&bytes).await?;
            stream.write_all(b"\r\n").await?;
            sent += bytes.len();
        }
        if done {
            return stream.write_all(b"0\r\n\r\n").await;
        }
        if tokio::time::timeout(STALL_TIMEOUT, grew).await.is_err() {
            return stream.write_all(b"0\r\n\r\n").await;
        }
    }
}

/// Serves `/metrics`, `/healthz` and the answers under `/speech/`. Anything else gets a 404.
pub async fn serve_http(listener: TcpListener, metrics: Arc<Metrics>, speech: Arc<SpeechClips>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let (metrics, speech) = (metrics.clone(), speech.clone());
        tokio::spawn(async move {
            // The whole request head, which clients may send in pieces; closing with unread
            // input would reset the connection under the response.
            let (mut request, mut n) = ([0u8; 2048], 0);
            while n < request.len() && !request[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut request[n..]).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => n += read,
                }
            }
            let path = request[..n].split(|&b| b == b' ').nth(1).unwrap_or_default();
            let clip = std::str::from_utf8(path)
                .ok()
                .and_then(|p| p.strip_prefix("/speech/")?.strip_suffix(".wav"))
                .and_then(|id| speech.get(id));
            if let Some(clip) = clip {
                let _ = send_clip(&mut stream, &clip).await;
                return;
            }
            let (status, kind, body): (&str, &str, Vec<u8>) = match path {
                b"/metrics" => ("200 OK", "text/plain; version=0.0.4", metrics.render().into_bytes()),
                b"/healthz" => ("200 OK", "text/plain", b"ok\n".to_vec()),
                _ => ("404 Not Found", "text/plain", b"not found\n".to_vec()),
            };
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(&body).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serves_a_stored_answer() {
        let speech = Arc::new(SpeechClips::default());
        let path = speech.put(&[1, -1, 2], 24_000);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_http(listener, Arc::new(Metrics::default()), speech));
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        // In pieces, like a client that writes unbuffered.
        for piece in ["GET ", &path, " HTTP/1.1\r\n", "Host: x\r\n\r\n"] {
            stream.write_all(piece.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{path}: {text}");
        assert!(text.contains("content-type: audio/wav") && response.ends_with(&[1, 0, 0xff, 0xff, 2, 0]));
    }

    #[tokio::test]
    async fn streams_a_growing_answer() {
        let speech = Arc::new(SpeechClips::default());
        let (path, clip) = speech.open(24_000);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_http(listener, Arc::new(Metrics::default()), speech));
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
        // The header arrives before any audio exists.
        let mut first = vec![0u8; 512];
        let n = stream.read(&mut first).await.unwrap();
        let head = String::from_utf8_lossy(&first[..n]).into_owned();
        assert!(head.contains("transfer-encoding: chunked"), "{head}");
        clip.push(&[1, -1]);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        clip.push(&[2]);
        clip.finish();
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        let mut all = first[..n].to_vec();
        all.extend(rest);
        let body = &all[all.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4..];
        // Undo the chunking.
        let (mut wav, mut at) = (Vec::new(), 0);
        loop {
            let line_end = at + body[at..].windows(2).position(|w| w == b"\r\n").unwrap();
            let len = usize::from_str_radix(std::str::from_utf8(&body[at..line_end]).unwrap(), 16).unwrap();
            if len == 0 {
                break;
            }
            wav.extend_from_slice(&body[line_end + 2..line_end + 2 + len]);
            at = line_end + 2 + len + 2;
        }
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[40..44], &u32::MAX.to_le_bytes(), "unknown length while streaming");
        assert_eq!(&wav[44..], &[1, 0, 0xff, 0xff, 2, 0]);
    }
}

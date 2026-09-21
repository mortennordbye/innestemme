use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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

/// Serves `/metrics` and `/healthz`. Anything else gets a 404.
pub async fn serve_http(listener: TcpListener, metrics: Arc<Metrics>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else { continue };
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut request = [0u8; 1024];
            let Ok(n) = stream.read(&mut request).await else { return };
            let (status, body) = match request[..n].split(|&b| b == b' ').nth(1) {
                Some(b"/metrics") => ("200 OK", metrics.render()),
                Some(b"/healthz") => ("200 OK", "ok\n".to_owned()),
                _ => ("404 Not Found", "not found\n".to_owned()),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/plain; version=0.0.4\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

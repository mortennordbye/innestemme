use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use tracing::info;
use voice_codec::MimiCodec;
use voice_engine::{
    metrics::serve_http, serve, Config, FrameProcessor, LoopbackEngine, Metrics, MimiProcessor, Passthrough,
    UdpTransport,
};

// Debug builds abort on any allocation inside a `no_alloc` section. Release builds use mimalloc
// for its tail latency; the guards compile to nothing there.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOC: voice_rt::noalloc::AllocDisabler = voice_rt::noalloc::AllocDisabler;

#[cfg(not(debug_assertions))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Clone, Copy, ValueEnum)]
enum Processor {
    /// Mimi encode -> loopback engine -> Mimi decode.
    Mimi,
    /// Echo PCM without touching a model.
    Passthrough,
}

#[derive(Parser)]
#[command(about = "Low-latency UDP voice engine")]
struct Args {
    #[arg(long, env = "VOICE_BIND", default_value = "0.0.0.0:7000")]
    bind: SocketAddr,
    #[arg(long, env = "VOICE_METRICS_BIND", default_value = "0.0.0.0:9090")]
    metrics_bind: SocketAddr,
    #[arg(long, env = "VOICE_PROCESSOR", value_enum, default_value = "mimi")]
    processor: Processor,
    /// Candle-format Mimi safetensors. Downloaded from Hugging Face when omitted.
    #[arg(long, env = "VOICE_MIMI_MODEL")]
    mimi_model: Option<PathBuf>,
    /// Compute threads for candle. Set this to the pod's CPU count: the default is the host's
    /// core count, which oversubscribes a pod with a smaller cpuset.
    #[arg(long, env = "VOICE_THREADS")]
    threads: Option<usize>,
    /// Core to pin the model thread to.
    #[arg(long, env = "VOICE_MODEL_CORE")]
    model_core: Option<usize>,
    /// Reordering tolerance in 20 ms packets.
    #[arg(long, env = "VOICE_JITTER_DEPTH", default_value_t = 2)]
    jitter_depth: u32,
    #[arg(long, env = "VOICE_SESSION_TIMEOUT_SECS", default_value_t = 5)]
    session_timeout_secs: u64,
    /// SO_BUSY_POLL in microseconds (Linux only).
    #[arg(long, env = "VOICE_BUSY_POLL_US")]
    busy_poll_us: Option<u32>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let args = Args::parse();

    let threads = args.threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    voice_rt::threads::configure_compute_threads(threads);
    info!(
        threads,
        avx = candle::utils::with_avx(),
        neon = candle::utils::with_neon(),
        f16c = candle::utils::with_f16c(),
        "compute configured"
    );

    let processor: Box<dyn FrameProcessor> = match args.processor {
        Processor::Passthrough => Box::new(Passthrough),
        Processor::Mimi => {
            let path = match args.mimi_model {
                Some(path) => path,
                None => MimiCodec::fetch_weights()?,
            };
            info!(path = %path.display(), "loading mimi");
            Box::new(MimiProcessor::new(MimiCodec::load(&path)?, LoopbackEngine))
        }
    };

    let cfg = Config {
        jitter_depth: args.jitter_depth,
        model_core: args.model_core,
        session_timeout: Duration::from_secs(args.session_timeout_secs),
    };

    // The network side is one task; a single-threaded runtime keeps it off the compute cores.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let metrics = Arc::new(Metrics::default());
        let http = tokio::net::TcpListener::bind(args.metrics_bind).await.context("binding metrics listener")?;
        tokio::spawn(serve_http(http, metrics.clone()));
        let transport = UdpTransport::bind(args.bind, 1 << 20, args.busy_poll_us).context("binding udp socket")?;
        info!(udp = %args.bind, metrics = %args.metrics_bind, "listening");
        serve(transport, processor, cfg, metrics, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    })
}

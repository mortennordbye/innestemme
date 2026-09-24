use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use tracing::info;
use voice_assistant::dialog::WakeWord;
use voice_assistant::lang::Lang;
use voice_assistant::pocket::PocketFiles;
use voice_assistant::stt::{SpeechToText, SttFiles};
use voice_assistant::whisper::{Transcriber, WakePrompt, WhisperFiles};
use voice_assistant::wyoming::WyomingTts;
use voice_codec::MimiCodec;
use voice_engine::assistant::{AssistantConfig, AssistantProcessor, Listener};
use voice_engine::settings;
use voice_engine::{
    metrics::{serve_http, SpeechClips},
    satellite::SatelliteConfig,
    serve, Config, FrameProcessor, LoopbackEngine, Metrics, MimiProcessor, Passthrough, UdpTransport,
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
    /// Voice assistant (wake word, default "Homie"): STT, weather lookup, spoken reply (macOS `say`).
    Assistant,
}

#[derive(Clone, Copy, ValueEnum)]
enum WakePromptArg {
    None,
    Name,
    Context,
}

#[derive(Clone, Copy, ValueEnum)]
enum EnglishTts {
    /// Kyutai Pocket TTS on the CPU, streaming.
    Pocket,
    /// macOS `say`.
    Say,
    /// Piper over Wyoming (`--piper`).
    Piper,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum NorwegianTts {
    /// macOS `say` (`--say-voice-no`).
    Say,
    /// Piper over Wyoming (`--piper`).
    Piper,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PocketPrecision {
    F32,
    /// 8-bit linear layers: smaller and usually faster on weak CPUs.
    Q8,
}

impl From<PocketPrecision> for voice_assistant::pocket::Precision {
    fn from(p: PocketPrecision) -> Self {
        match p {
            PocketPrecision::F32 => Self::F32,
            PocketPrecision::Q8 => Self::Q8,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ListenerKind {
    /// NB-Whisper per utterance: English and Norwegian.
    Whisper,
    /// Kyutai STT-1B streaming: English (and French) only, lower latency.
    Kyutai,
}

#[derive(Parser)]
#[command(about = "Low-latency UDP voice engine")]
struct Args {
    /// Settings file (YAML, e.g. a mounted ConfigMap): `option-name: value` for any option below.
    /// Command line and environment win over it; `X_FILE` reads the variable `X` from a file.
    #[arg(long, env = settings::CONFIG_ENV)]
    config: Option<PathBuf>,
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
    /// How the assistant listens.
    #[arg(long, env = "VOICE_LISTENER", value_enum, default_value = "whisper")]
    listener: ListenerKind,
    /// Also understand and answer Norwegian. Off: English only, and the language-ID model
    /// (openai/whisper-tiny) is not loaded.
    #[arg(long, env = "VOICE_NORWEGIAN", default_value_t = false, action = clap::ArgAction::Set)]
    norwegian: bool,
    /// The assistant's name: saying it wakes the assistant.
    #[arg(long, env = "VOICE_WAKE_NAME", default_value = "Homie")]
    wake_name: String,
    /// Other ways the transcriber writes the name, comma-separated (see DUMP / `--dump-utterances`).
    #[arg(long, env = "VOICE_WAKE_SPELLINGS", value_delimiter = ',')]
    wake_spellings: Vec<String>,
    /// How Whisper is taught the name's spelling. Default: `context` for OpenAI checkpoints,
    /// `name` for NB-Whisper.
    #[arg(long, env = "VOICE_WAKE_PROMPT", value_enum)]
    wake_prompt: Option<WakePromptArg>,
    /// Write every detected utterance here as a wav, for tuning recognition on real voices.
    #[arg(long, env = "VOICE_DUMP_UTTERANCES")]
    dump_utterances: Option<PathBuf>,
    /// Whisper size: base (290 MB) or small (970 MB, more accurate).
    #[arg(long, env = "VOICE_WHISPER_SIZE", default_value = "base")]
    whisper_size: String,
    /// Hugging Face repo of the Whisper model, overriding the size-based default (openai/whisper-*,
    /// or NbAiLab/nb-whisper-* with Norwegian on).
    #[arg(long, env = "VOICE_WHISPER_REPO")]
    whisper_repo: Option<String>,
    /// Run the assistant's STT on the Apple GPU. Needs a build with `--features metal`.
    #[arg(long, env = "VOICE_STT_GPU")]
    stt_gpu: bool,
    /// Home Assistant base URL, for example http://homeassistant.local:8123. Lights need this and a token.
    #[arg(long, env = "HA_URL")]
    ha_url: Option<String>,
    /// Home Assistant long-lived access token (Profile > Security in the HA UI).
    #[arg(long, env = "HA_TOKEN", hide_env_values = true)]
    ha_token: Option<String>,
    /// Place for weather questions that name none.
    #[arg(long, env = "VOICE_HOME")]
    home: Option<String>,
    /// Speech engine for English replies.
    #[arg(long, env = "VOICE_ENGLISH_TTS", value_enum, default_value = "pocket")]
    english_tts: EnglishTts,
    /// Pocket TTS preset voice: alba, marius, javert, jean, fantine, cosette, eponine, azelma.
    #[arg(long, env = "VOICE_POCKET_VOICE", default_value = "cosette")]
    pocket_voice: String,
    #[arg(long, env = "VOICE_POCKET_PRECISION", value_enum, default_value = "f32")]
    pocket_precision: PocketPrecision,
    /// macOS `say` voice for English replies with `--english-tts say`; the system default when omitted.
    #[arg(long, env = "VOICE_SAY_VOICE")]
    say_voice: Option<String>,
    /// Speech engine for Norwegian replies. Default: Piper when `--piper` is set, else macOS `say`.
    #[arg(long, env = "VOICE_NORWEGIAN_TTS", value_enum)]
    norwegian_tts: Option<NorwegianTts>,
    /// Wyoming Piper server, host:port: Home Assistant's Piper add-on (with its port exposed), a
    /// sidecar container, or a local `rhasspy/wyoming-piper`.
    #[arg(long, env = "VOICE_PIPER")]
    piper: Option<String>,
    /// Piper voice for Norwegian replies.
    #[arg(long, env = "VOICE_PIPER_VOICE_NO", default_value = "no_NO-talesyntese-medium")]
    piper_voice_no: String,
    /// Piper voice for English replies with `--english-tts piper`; the server's default when omitted.
    #[arg(long, env = "VOICE_PIPER_VOICE_EN")]
    piper_voice_en: Option<String>,
    /// macOS `say` voice for Norwegian replies.
    #[arg(long, env = "VOICE_SAY_VOICE_NO", default_value = "Nora")]
    say_voice_no: String,
    /// Music Assistant player that music plays on: its name or its media_player entity id.
    /// Optional when Music Assistant has exactly one player.
    #[arg(long, env = "VOICE_SPEAKER")]
    speaker: Option<String>,
    /// The room the microphone is in ("Living Room"), for "turn off the lights" without a room.
    /// Default for a satellite: its area in Home Assistant.
    #[arg(long, env = "VOICE_ROOM")]
    room: Option<String>,
    /// OpenAI-compatible API for requests the rules do not recognise, e.g.
    /// http://127.0.0.1:11434/v1 (Ollama). Off when unset.
    #[arg(long, env = "VOICE_LLM_URL")]
    llm_url: Option<String>,
    /// Model name at `--llm-url`. Qwen3 4B instruct measured best on CPU (see docs/benchmarks.md).
    #[arg(long, env = "VOICE_LLM_MODEL", default_value = "qwen3:4b-instruct")]
    llm_model: String,
    /// API key for `--llm-url`, when it needs one.
    #[arg(long, env = "VOICE_LLM_KEY", hide_env_values = true)]
    llm_key: Option<String>,
    /// Give up on the language model after this long and answer from the rules.
    #[arg(long, env = "VOICE_LLM_TIMEOUT_SECS", default_value_t = 15)]
    llm_timeout_secs: u64,
    /// ESPHome voice satellite to be the voice assistant for, host:port (a Voice PE: its IP and
    /// 6053). Disable the device's Assist satellite entity in Home Assistant first: a device
    /// streams to one voice assistant only.
    #[arg(long, env = "VOICE_SATELLITE")]
    satellite: Option<String>,
    /// The satellite's API encryption key (base64), when it has one.
    #[arg(long, env = "VOICE_SATELLITE_KEY", hide_env_values = true)]
    satellite_key: Option<String>,
    /// Base URL a satellite fetches spoken answers from, e.g. http://voice.lan:9090. Default: this
    /// host's address toward the device and the metrics port.
    #[arg(long, env = "VOICE_PUBLIC_URL")]
    public_url: Option<String>,
    /// Start playing a spoken answer on a satellite that fetches a URL while it is still being
    /// synthesized, like Home Assistant does; `false` serves each answer once it is complete.
    #[arg(long, env = "VOICE_SATELLITE_STREAM", default_value_t = true, action = clap::ArgAction::Set)]
    satellite_stream: bool,
    /// Download and load the models, then exit.
    #[arg(long)]
    download_only: bool,
    /// SO_BUSY_POLL in microseconds (Linux only).
    #[arg(long, env = "VOICE_BUSY_POLL_US")]
    busy_poll_us: Option<u32>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let raw: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let layered = settings::layered(&Args::command(), &raw, &|var| std::env::var(var).ok())?;
    let args = Args::from_arg_matches(&layered.apply(Args::command()).get_matches_from(raw))?;
    for (name, source) in layered.sources() {
        info!(name, source, "setting");
    }

    let threads = args.threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    voice_rt::threads::configure_compute_threads(threads);
    voice_assistant::pocket::set_threads(threads);
    info!(
        threads,
        avx = candle::utils::with_avx(),
        neon = candle::utils::with_neon(),
        f16c = candle::utils::with_f16c(),
        "compute configured"
    );

    let mimi_path = || match args.mimi_model.clone() {
        Some(path) => Ok(path),
        None => MimiCodec::fetch_weights(),
    };
    let mut handle = None;
    let processor: Box<dyn FrameProcessor> = match args.processor {
        Processor::Passthrough => Box::new(Passthrough),
        Processor::Mimi => {
            let path = mimi_path()?;
            info!(path = %path.display(), "loading mimi");
            Box::new(MimiProcessor::new(MimiCodec::load(&path)?, LoopbackEngine))
        }
        Processor::Assistant => {
            let device = voice_assistant::stt::device(args.stt_gpu)?;
            let listener = match args.listener {
                ListenerKind::Whisper => {
                    let repo = args
                        .whisper_repo
                        .clone()
                        .unwrap_or_else(|| voice_assistant::whisper::default_repo(args.norwegian, &args.whisper_size));
                    let files = WhisperFiles::fetch(&repo)?;
                    info!(model = %files.model.display(), ?device, "loading whisper");
                    let language_id = match args.norwegian {
                        true => Some(WhisperFiles::fetch(voice_assistant::whisper::LANGUAGE_ID_REPO)?),
                        false => None,
                    };
                    let transcriber = Transcriber::load(&files, language_id.as_ref(), device)?;
                    let prompt = match args.wake_prompt {
                        Some(WakePromptArg::None) => WakePrompt::None,
                        Some(WakePromptArg::Name) => WakePrompt::Name,
                        Some(WakePromptArg::Context) => WakePrompt::Context,
                        None if repo.starts_with("openai/") => WakePrompt::Context,
                        None => WakePrompt::Name,
                    };
                    info!(?prompt, "wake word prompt");
                    let mut transcriber = transcriber.with_wake_prompt(prompt, &args.wake_name);
                    info!(prompt = transcriber.prompt_text(), "whisper prompt");
                    if !args.norwegian {
                        transcriber.restrict(Lang::English);
                    }
                    Listener::Whisper(Box::new(transcriber))
                }
                ListenerKind::Kyutai => {
                    let files = SttFiles::fetch()?;
                    info!(model = %files.model.display(), ?device, "loading stt");
                    Listener::Kyutai(Box::new(SpeechToText::load(&files, &mimi_path()?, device)?))
                }
            };
            let say =
                || voice_assistant::tts::Say { english: args.say_voice.clone(), norwegian: args.say_voice_no.clone() };
            let piper = || -> Result<WyomingTts> {
                let address = args.piper.clone().context("Piper speech needs `--piper host:port`")?;
                let piper = WyomingTts {
                    address,
                    english_voice: args.piper_voice_en.clone(),
                    norwegian_voice: Some(args.piper_voice_no.clone()),
                };
                let voices = piper.voices().context("asking the Piper server for its voices")?;
                for wanted in [piper.english_voice.as_ref(), piper.norwegian_voice.as_ref()].into_iter().flatten() {
                    match voices.iter().find(|(name, _)| name == wanted) {
                        Some((_, true)) => {}
                        // wyoming-piper downloads a known voice on first use.
                        Some((_, false)) => {
                            info!(voice = wanted, "piper voice downloads on first use")
                        }
                        None => anyhow::bail!("Piper at {} has no voice {wanted}", piper.address),
                    }
                }
                info!(address = piper.address, voices = voices.len(), "piper connected");
                Ok(piper)
            };
            let english: Box<dyn voice_assistant::tts::Tts> = match args.english_tts {
                EnglishTts::Pocket => {
                    let files = PocketFiles::fetch(&args.pocket_voice)?;
                    info!(model = %files.model.display(), voice = args.pocket_voice, precision = ?args.pocket_precision, "loading pocket tts");
                    voice_assistant::pocket::load(&files, args.pocket_precision.into())?
                }
                EnglishTts::Say => Box::new(say()),
                EnglishTts::Piper => Box::new(piper()?),
            };
            let norwegian: Box<dyn voice_assistant::tts::Tts> = match args.norwegian_tts {
                Some(NorwegianTts::Piper) => Box::new(piper()?),
                None if args.piper.is_some() && args.norwegian => Box::new(piper()?),
                _ => Box::new(say()),
            };
            let tts = Box::new(voice_assistant::tts::ByLanguage { english, norwegian });
            let assistant = AssistantProcessor::new(
                listener,
                tts,
                AssistantConfig {
                    home: args.home.clone(),
                    home_assistant: args.ha_url.clone().zip(args.ha_token.clone()),
                    dump_utterances: args.dump_utterances.clone(),
                    speaker: args.speaker.clone(),
                    room: args.room.clone(),
                    llm: args.llm_url.as_deref().map(|url| {
                        info!(url, model = args.llm_model, "language model");
                        voice_assistant::llm::Llm::new(
                            url,
                            &args.llm_model,
                            args.llm_key.clone(),
                            Duration::from_secs(args.llm_timeout_secs),
                        )
                    }),
                    wake: WakeWord::new(&args.wake_name).with_spellings(&args.wake_spellings),
                },
            )?;
            handle = Some(assistant.handle());
            Box::new(assistant)
        }
    };

    if args.download_only {
        info!("models downloaded and loaded");
        return Ok(());
    }

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
        let speech = Arc::new(SpeechClips::default());
        tokio::spawn(serve_http(http, metrics.clone(), speech.clone()));
        if let Some(address) = args.satellite.clone() {
            let assistant = handle.context("`--satellite` needs `--processor assistant`")?;
            let key = args.satellite_key.as_deref().map(voice_esphome::frame::parse_key).transpose()?;
            let engine = SocketAddr::new(
                if args.bind.ip().is_unspecified() { [127, 0, 0, 1].into() } else { args.bind.ip() },
                args.bind.port(),
            );
            let cfg = SatelliteConfig {
                address,
                key,
                engine,
                public_url: args.public_url.clone(),
                http_port: args.metrics_bind.port(),
                stream_answers: args.satellite_stream,
            };
            tokio::spawn(voice_engine::satellite::run(cfg, assistant, speech));
        }
        let transport = UdpTransport::bind(args.bind, 1 << 20, args.busy_poll_us).context("binding udp socket")?;
        info!(udp = %args.bind, metrics = %args.metrics_bind, "listening");
        serve(transport, processor, cfg, metrics, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    })
}

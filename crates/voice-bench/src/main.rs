//! M0 benchmark: per-frame step time of the speech stack on this host. Prints markdown table rows
//! for `docs/benchmarks.md` on stdout and progress on stderr.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use candle::{DType, Device};
use candle_transformers::generation::{LogitsProcessor, Sampling};
use clap::{Parser, ValueEnum};
use moshi::lm_generate_multistream::{Config as GenConfig, State};
use voice_codec::mimi::{MimiCodec, CODEBOOKS};
use voice_proto::{FRAME_SAMPLES, SAMPLE_RATE};

// Same allocator as the release engine, so candle's per-op allocations cost the same here.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

const FRAME: Duration = Duration::from_millis(80);
const MOSHI_REPO: &str = "kyutai/moshiko-candle-q8";
const MOSHI_FILE: &str = "model.q8.gguf";

#[derive(Clone, Copy, ValueEnum)]
enum Bench {
    /// Mimi encode + decode only.
    Mimi,
    /// Mimi encode -> Moshi 7B q8 step -> Mimi decode. Downloads ~8 GB and needs ~10 GB of RAM.
    Moshi,
    /// Kyutai Pocket TTS on the CPU: time to first audio, synthesis speed. Downloads ~240 MB.
    Pocket,
    /// Kyutai STT-1B streaming step (its own 32-codebook Mimi encode + LM). Downloads ~2 GB.
    /// Prints the transcript on stderr, so a spoken `--wav-in` doubles as a recognition check.
    Stt,
}

#[derive(Parser)]
#[command(about = "Per-frame step time of the speech stack, as markdown table rows")]
struct Args {
    #[arg(long, value_enum, default_value = "mimi")]
    bench: Bench,
    /// First column of the table, for example the hypervisor name.
    #[arg(long, env = "VOICE_BENCH_LABEL", default_value = "unlabelled")]
    label: String,
    /// Compute threads for candle. Set this to the pod's CPU count.
    #[arg(long, env = "VOICE_THREADS")]
    threads: Option<usize>,
    /// Measured 80 ms frames.
    #[arg(long, default_value_t = 250)]
    frames: usize,
    /// Unmeasured frames run first. Covers the priming frames of the streaming codec.
    #[arg(long, default_value_t = 25)]
    warmup: usize,
    /// 24 kHz mono 16-bit wav used as input, looped. Default is a synthetic signal.
    #[arg(long)]
    wav_in: Option<PathBuf>,
    #[arg(long, env = "VOICE_MIMI_MODEL")]
    mimi_model: Option<PathBuf>,
    #[arg(long, env = "VOICE_MOSHI_MODEL")]
    moshi_model: Option<PathBuf>,
    /// Pocket TTS preset voice.
    #[arg(long, default_value = "cosette")]
    voice: String,
    /// Pocket TTS: also write the last utterance here (24 kHz mono wav).
    #[arg(long)]
    wav_out: Option<PathBuf>,
    /// Pocket TTS weights: f32 or q8.
    #[arg(long, default_value = "f32")]
    precision: String,
    /// Run the STT model on the Apple GPU (needs `--features metal`).
    #[arg(long)]
    gpu: bool,
    /// Print the table header before the rows.
    #[arg(long)]
    header: bool,
}

#[derive(Default)]
struct Series(Vec<Duration>);

impl Series {
    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1e3
    }

    /// `None` when nothing was recorded.
    fn row(&mut self, prefix: &str, stage: &str) -> Option<String> {
        self.0.sort_unstable();
        let n = self.0.len();
        let max = *self.0.last()?;
        let mean = self.0.iter().sum::<Duration>() / n as u32;
        let at = |q: f64| self.0[((n as f64 * q).ceil() as usize).clamp(1, n) - 1];
        Some(format!(
            "| {prefix} | {stage} | {n} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |",
            Self::ms(mean),
            Self::ms(at(0.5)),
            Self::ms(at(0.99)),
            Self::ms(max),
            FRAME.as_secs_f64() / mean.as_secs_f64(),
        ))
    }
}

fn cpu_name() -> String {
    let from_proc = std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|info| {
        let line = info.lines().find(|l| l.starts_with("model name"))?;
        Some(line.split_once(':')?.1.trim().to_owned())
    });
    let from_sysctl = || {
        let out = std::process::Command::new("sysctl").args(["-n", "machdep.cpu.brand_string"]).output().ok()?;
        Some(String::from_utf8(out.stdout).ok()?.trim().to_owned())
    };
    from_proc.or_else(from_sysctl).filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".into())
}

fn simd() -> String {
    let flags = [
        ("avx", candle::utils::with_avx()),
        ("f16c", candle::utils::with_f16c()),
        ("neon", candle::utils::with_neon()),
    ];
    let on: Vec<_> = flags.iter().filter(|(_, on)| *on).map(|(name, _)| *name).collect();
    if on.is_empty() {
        "scalar".into()
    } else {
        on.join("+")
    }
}

/// Voiced-speech-like test signal: a 140 Hz harmonic stack under a 4 Hz syllable envelope, plus
/// noise. Mimi's cost does not depend on the content, this only keeps the input out of the
/// digital-silence corner.
fn synthetic(frames: usize) -> Vec<i16> {
    let mut noise = 0x2545_f491u32;
    (0..frames * FRAME_SAMPLES)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            let voiced: f32 = (1..=8).map(|h| (t * 140.0 * h as f32 * std::f32::consts::TAU).sin() / h as f32).sum();
            let envelope = 0.5 - 0.5 * (t * 4.0 * std::f32::consts::TAU).cos();
            noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let hiss = (noise >> 16) as f32 / 32768.0 - 1.0;
            ((voiced * envelope * 0.3 + hiss * 0.02) * 32767.0) as i16
        })
        .collect()
}

fn read_wav(path: &Path) -> Result<Vec<i16>> {
    let mut reader = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != SAMPLE_RATE || spec.channels != 1 || spec.bits_per_sample != 16 {
        bail!("{} must be 24 kHz mono 16-bit, got {spec:?}", path.display());
    }
    let pcm: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;
    if pcm.len() < FRAME_SAMPLES {
        bail!("{} is shorter than one 80 ms frame", path.display());
    }
    Ok(pcm)
}

fn load_moshi(path: Option<PathBuf>, steps: usize) -> Result<State> {
    let path = match path {
        Some(path) => path,
        // `Api::new()` ignores HF_HOME.
        None => hf_hub::api::sync::ApiBuilder::from_env()
            .build()?
            .model(MOSHI_REPO.to_owned())
            .get(MOSHI_FILE)
            .with_context(|| format!("fetching {MOSHI_REPO}/{MOSHI_FILE}"))?,
    };
    eprintln!("loading moshi from {}", path.display());
    let lm = moshi::lm::load_streaming(&path, DType::F32, &Device::Cpu)
        .with_context(|| format!("loading moshi weights from {}", path.display()))?;
    // Sampling settings of moshi-backend's defaults; they do not affect the step time.
    let audio_lp = LogitsProcessor::from_sampling(299_792_458, Sampling::TopK { k: 250, temperature: 0.8 });
    let text_lp = LogitsProcessor::from_sampling(299_792_458, Sampling::TopK { k: 25, temperature: 0.8 });
    Ok(State::new(lm, steps, audio_lp, text_lp, None, None, None, GenConfig::v0_1()))
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.frames == 0 {
        bail!("--frames must be at least 1");
    }
    let threads = args.threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    voice_rt::threads::configure_compute_threads(threads);

    let total = args.warmup + args.frames;
    let pcm = match &args.wav_in {
        Some(path) => read_wav(path)?,
        None => synthetic(total),
    };
    let mut input = pcm.as_chunks::<FRAME_SAMPLES>().0.iter().cycle();

    let mimi_path = match args.mimi_model.clone() {
        Some(path) => path,
        None => MimiCodec::fetch_weights()?,
    };
    if matches!(args.bench, Bench::Pocket) {
        return bench_pocket(&args, threads);
    }
    if matches!(args.bench, Bench::Stt) {
        return bench_stt(&args, threads, &mimi_path, &mut input, total);
    }
    let mut codec = MimiCodec::load(&mimi_path)?;
    let mut moshi = match args.bench {
        Bench::Mimi | Bench::Stt | Bench::Pocket => None,
        Bench::Moshi => Some(load_moshi(args.moshi_model, total + 1)?),
    };
    let mut text_token = GenConfig::v0_1().text_start_token;

    let (mut encode, mut step, mut decode, mut whole) =
        (Series::default(), Series::default(), Series::default(), Series::default());
    for i in 0..total {
        let measured = i >= args.warmup;
        let frame = input.next().context("no input frames")?;

        let t0 = Instant::now();
        let codes = codec.encode(frame)?;
        let t1 = Instant::now();
        let reply: Option<[u32; CODEBOOKS]> = match (&mut moshi, codes) {
            (None, codes) => codes,
            (Some(_), None) => None,
            (Some(state), Some(codes)) => {
                text_token = state.step_without_ca_src(text_token, &codes, None)?;
                match state.last_audio_tokens() {
                    Some(tokens) => Some(tokens[..CODEBOOKS].try_into()?),
                    None => None,
                }
            }
        };
        let t2 = Instant::now();
        let decoded = match &reply {
            Some(reply) => codec.decode(reply)?.is_some(),
            None => false,
        };
        let t3 = Instant::now();

        if measured {
            if codes.is_none() || !decoded {
                bail!("frame {i} was not fully processed; raise --warmup past the priming frames");
            }
            encode.0.push(t1 - t0);
            step.0.push(t2 - t1);
            decode.0.push(t3 - t2);
            whole.0.push(t3 - t0);
        }
        if i % 25 == 0 {
            eprintln!("frame {i}/{total}");
        }
    }

    if args.header {
        println!("| host | cpu | threads | simd | stage | frames | mean ms | p50 ms | p99 ms | max ms | RTF |");
        println!("|---|---|---|---|---|---|---|---|---|---|---|");
    }
    let prefix = format!("{} | {} | {threads} | {}", args.label, cpu_name(), simd());
    let mut rows = vec![(&mut encode, "mimi encode"), (&mut decode, "mimi decode")];
    if moshi.is_some() {
        rows.insert(1, (&mut step, "moshi 7B q8 step"));
    }
    rows.push((&mut whole, "whole frame"));
    for (series, stage) in rows {
        println!("{}", series.row(&prefix, stage).context("nothing measured")?);
    }
    Ok(())
}

fn bench_stt<'a>(
    args: &Args,
    threads: usize,
    mimi_path: &Path,
    input: &mut impl Iterator<Item = &'a [i16; FRAME_SAMPLES]>,
    total: usize,
) -> Result<()> {
    let files = voice_assistant::stt::SttFiles::fetch()?;
    eprintln!("loading stt from {}", files.model.display());
    let mut stt = voice_assistant::stt::SpeechToText::load(&files, mimi_path, voice_assistant::stt::device(args.gpu)?)?;
    let mut step = Series::default();
    let mut transcript = Vec::new();
    for i in 0..total {
        let frame = input.next().context("no input frames")?;
        let t0 = Instant::now();
        let out = stt.step(frame)?;
        if i >= args.warmup {
            step.0.push(t0.elapsed());
        }
        transcript.extend(out.words);
    }
    eprintln!("transcript: {}", transcript.join(" "));
    if args.header {
        println!("| host | cpu | threads | simd | stage | frames | mean ms | p50 ms | p99 ms | max ms | RTF |");
        println!("|---|---|---|---|---|---|---|---|---|---|---|");
    }
    let simd = if args.gpu { "metal".to_owned() } else { simd() };
    let prefix = format!("{} | {} | {threads} | {simd}", args.label, cpu_name());
    println!("{}", step.row(&prefix, "stt-1b step (mimi 32 cb encode + lm)").context("nothing measured")?);
    Ok(())
}

fn bench_pocket(args: &Args, threads: usize) -> Result<()> {
    use voice_assistant::pocket::{PocketFiles, Precision};
    const TEXT: &str = "Right now in Oslo it is 15 degrees and overcast, with wind at 3 meters per second. \
                        Tomorrow looks wetter, so bring an umbrella.";
    voice_assistant::pocket::set_threads(threads);
    let precision = match args.precision.as_str() {
        "f32" => Precision::F32,
        "q8" => Precision::Q8,
        other => bail!("--precision must be f32 or q8, got {other}"),
    };
    let files = PocketFiles::fetch(&args.voice)?;
    let mut tts = voice_assistant::pocket::load(&files, precision)?;
    let lang = voice_assistant::lang::Lang::English;
    tts.speak("Warming up.", lang, &mut |_| {})?;
    let (mut first, mut total, mut audio) = (Series::default(), Series::default(), 0usize);
    let mut last = Vec::new();
    let runs = args.frames.clamp(1, 20);
    for _ in 0..runs {
        let start = Instant::now();
        let mut first_at = None;
        last.clear();
        tts.speak(TEXT, lang, &mut |pcm| {
            first_at.get_or_insert_with(|| start.elapsed());
            audio += pcm.len();
            last.extend_from_slice(pcm);
        })?;
        first.0.push(first_at.context("no audio produced")?);
        total.0.push(start.elapsed());
    }
    if let Some(path) = &args.wav_out {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec)?;
        last.iter().try_for_each(|&s| writer.write_sample(s))?;
        writer.finalize()?;
    }
    let audio_s = audio as f64 / f64::from(SAMPLE_RATE) / runs as f64;
    let synth_s = total.0.iter().sum::<Duration>().as_secs_f64() / runs as f64;
    eprintln!("{runs} runs, {audio_s:.1} s of audio each, {:.1}x faster than real time", audio_s / synth_s);
    if args.header {
        println!("| host | cpu | threads | precision | stage | runs | mean ms | p50 ms | p99 ms | max ms | RTF |");
        println!("|---|---|---|---|---|---|---|---|---|---|---|");
    }
    let prefix = format!("{} | {} | {threads} | {}", args.label, cpu_name(), args.precision);
    println!("{}", first.row(&prefix, "pocket first audio").context("nothing measured")?);
    println!("{}", total.row(&prefix, "pocket whole utterance").context("nothing measured")?);
    Ok(())
}

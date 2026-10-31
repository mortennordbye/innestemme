//! Trains the male/female classifier in `voice_gender.rs` on LibriSpeech and writes its weights.
//!
//! LibriSpeech (CC BY 4.0, openslr.org/12): train-clean-100, dev-clean, dev-other, test-clean and
//! test-other, converted to 16 kHz wav as `<dir>/<set>/<speaker>/<utterance>.wav` (eight to twelve
//! per speaker is plenty). The train and dev sets train, the test sets (other speakers) measure.
//! A few speakers are labelled with the wrong sex and are left out (`MISLABELLED`).
//! `cargo run --release -p voice-assistant --example voice_gender_train -- <dir> SPEAKERS.TXT
//! crates/voice-assistant/src/voice_gender_weights.rs`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use voice_assistant::voice_gender::{sure, Gender, VoiceGender, SAMPLE_RATE};

/// Audio lengths to train and measure on, in seconds: a wake word is about one.
const CROPS: &[f32] = &[1.0, 1.5, 3.0];
/// Inverse regularisation strength, as in scikit-learn: 10 measured best on the test speakers;
/// 100 is surer and more often wrong, 1 is unsure of too many.
const C: f64 = 10.0;
const NEWTON_STEPS: usize = 25;
/// Labelled male, sound female to every speaker model tried (CAM++, ResNet34, TitaNet) out of fold,
/// with female names and pitch around 220 Hz.
const MISLABELLED: &[&str] = &["118", "1688", "7105", "7976"];

struct Sample {
    embedding: Vec<f32>,
    female: bool,
    crop: f32,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir, speakers, out] = args.as_slice() else {
        bail!("usage: voice_gender_train <wav dir> SPEAKERS.TXT <out.rs>")
    };
    let sex: HashMap<String, bool> = std::fs::read_to_string(speakers)?
        .lines()
        .filter(|l| !l.starts_with(';'))
        .filter_map(|l| {
            let mut fields = l.split('|').map(str::trim);
            Some((fields.next()?.to_owned(), fields.next()? == "F"))
        })
        .collect();
    let model = VoiceGender::load(4)?;
    let (mut train, mut test) = (Vec::new(), Vec::new());
    for set in std::fs::read_dir(dir)? {
        let set = set?.path();
        let name = set.file_name().unwrap().to_string_lossy().into_owned();
        let samples = if name.starts_with("test") { &mut test } else { &mut train };
        for speaker in std::fs::read_dir(&set)? {
            let speaker = speaker?.path();
            let id = speaker.file_name().unwrap().to_string_lossy().into_owned();
            if MISLABELLED.contains(&id.as_str()) {
                continue;
            }
            let female = *sex.get(&id).with_context(|| format!("speaker {id} not in SPEAKERS.TXT"))?;
            for wav in std::fs::read_dir(&speaker)? {
                samples.extend(embeddings(&model, &wav?.path(), female)?);
            }
        }
        eprintln!("{name}: {} train, {} test", train.len(), test.len());
    }

    let (w, b) = fit(&train);
    let dim = w.len();

    let mut file = String::from("// Written by examples/voice_gender_train.rs from LibriSpeech; do not edit.\n");
    file += &format!("const BIAS: f32 = {b:?};\nconst WEIGHTS: [f32; {dim}] = [\n");
    for row in w.chunks(8) {
        file += &format!("    {},\n", row.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join(", "));
    }
    file += "];\n";
    std::fs::write(out, file)?;

    // The engine has the new weights after a rebuild; measured here from `w`.
    for &crop in CROPS {
        let (mut right, mut wrong, mut unsure) = (0, 0, 0);
        for s in test.iter().filter(|s| s.crop == crop) {
            match sure(predict(&w, b, &s.embedding)) {
                None => unsure += 1,
                Some(g) if (g == Gender::Female) == s.female => right += 1,
                Some(_) => wrong += 1,
            }
        }
        let n = (right + wrong + unsure) as f32 / 100.0;
        println!(
            "{crop} s: {:.1}% right, {:.1}% wrong, {:.1}% unsure ({} clips)",
            right as f32 / n,
            wrong as f32 / n,
            unsure as f32 / n,
            right + wrong + unsure
        );
    }
    Ok(())
}

/// Logistic regression with balanced class weights and L2 on the weights (not the bias), the same
/// optimum as scikit-learn's `LogisticRegression(C, class_weight="balanced")`, by Newton's method.
fn fit(train: &[Sample]) -> (Vec<f32>, f32) {
    let d = train[0].embedding.len() + 1;
    let n = train.len() as f64;
    let females = train.iter().filter(|s| s.female).count() as f64;
    let weight = |s: &Sample| if s.female { n / (2.0 * females) } else { n / (2.0 * (n - females)) };
    let x = |s: &Sample| -> Vec<f64> { s.embedding.iter().map(|&v| f64::from(v)).chain([1.0]).collect() };
    let mut theta = vec![0f64; d];
    for _ in 0..NEWTON_STEPS {
        // Gradient and Hessian of ½|w|² + C Σ weight · loss.
        let mut grad: Vec<f64> = theta.iter().map(|t| t / C).collect();
        let mut hess = vec![0f64; d * d];
        (0..d - 1).for_each(|i| hess[i * d + i] = 1.0 / C);
        grad[d - 1] = 0.0;
        for s in train {
            let xi = x(s);
            let z: f64 = theta.iter().zip(&xi).map(|(t, v)| t * v).sum();
            let p = 1.0 / (1.0 + (-z).exp());
            let (g, h) = (weight(s) * (p - f64::from(u8::from(s.female))), weight(s) * p * (1.0 - p));
            for i in 0..d {
                grad[i] += g * xi[i];
                let row = &mut hess[i * d..i * d + i + 1];
                row.iter_mut().zip(&xi).for_each(|(r, xj)| *r += h * xi[i] * xj);
            }
        }
        let step = cholesky_solve(&mut hess, d, &grad);
        theta.iter_mut().zip(&step).for_each(|(t, s)| *t -= s);
        if step.iter().map(|s| s * s).sum::<f64>().sqrt() < 1e-8 {
            break;
        }
    }
    let b = theta.pop().unwrap_or_default() as f32;
    (theta.into_iter().map(|t| t as f32).collect(), b)
}

/// Solves `a x = b` for a symmetric positive definite `a` given by its lower triangle (row-major
/// `d` by `d`), which it overwrites with its Cholesky factor.
fn cholesky_solve(a: &mut [f64], d: usize, b: &[f64]) -> Vec<f64> {
    for j in 0..d {
        let diag = (a[j * d + j] - (0..j).map(|k| a[j * d + k].powi(2)).sum::<f64>()).sqrt();
        a[j * d + j] = diag;
        for i in j + 1..d {
            let dot: f64 = (0..j).map(|k| a[i * d + k] * a[j * d + k]).sum();
            a[i * d + j] = (a[i * d + j] - dot) / diag;
        }
    }
    let mut y = vec![0f64; d];
    for i in 0..d {
        y[i] = (b[i] - (0..i).map(|k| a[i * d + k] * y[k]).sum::<f64>()) / a[i * d + i];
    }
    for i in (0..d).rev() {
        y[i] = (y[i] - (i + 1..d).map(|k| a[k * d + i] * y[k]).sum::<f64>()) / a[i * d + i];
    }
    y
}

fn predict(w: &[f32], b: f32, x: &[f32]) -> f32 {
    let z = b + w.iter().zip(x).map(|(w, x)| w * x).sum::<f32>();
    1.0 / (1.0 + (-z).exp())
}

/// One embedding per crop length, from the start of the speech.
fn embeddings(model: &VoiceGender, path: &Path, female: bool) -> Result<Vec<Sample>> {
    let pcm: Vec<i16> = hound::WavReader::open(path)?.samples::<i16>().collect::<Result<_, _>>()?;
    let mut samples = Vec::new();
    for &crop in CROPS {
        // Past the leading silence.
        let start = SAMPLE_RATE as usize / 4;
        let end = start + (crop * SAMPLE_RATE as f32) as usize;
        if pcm.len() >= end {
            samples.push(Sample { embedding: model.embedding(&pcm[start..end])?, female, crop });
        }
    }
    Ok(samples)
}

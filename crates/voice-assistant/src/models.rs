//! Model files, downloaded once into the Hugging Face cache (`$HF_HOME`, else
//! `~/.cache/huggingface`): from a Hugging Face repo, or from a release with a known SHA-256.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use hf_hub::api::sync::{ApiBuilder, ApiRepo};
use hf_hub::{Repo, RepoType};
use sha2::{Digest, Sha256};

pub(crate) fn cache_dir() -> PathBuf {
    match std::env::var_os("HF_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/huggingface"),
    }
}

/// The file from the release at `release`, downloaded once and checked against its hash.
pub(crate) fn fetch_from(dir: &Path, release: &str, (file, sha256): (&str, &str)) -> Result<PathBuf> {
    let path = dir.join(file);
    if path.exists() {
        return Ok(path);
    }
    let url = format!("{release}/{file}");
    let bytes = ureq::get(&url)
        .call()
        // ureq stops at 10 MB unless told otherwise; the speaker model is 30 MB.
        .and_then(|mut response| response.body_mut().with_config().limit(200 << 20).read_to_vec())
        .with_context(|| format!("downloading {url}"))?;
    let digest: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if digest != sha256 {
        bail!("{url}: SHA-256 {digest}, expected {sha256}");
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Written under another name first, so an interrupted download is not taken for the model.
    crate::store::write_atomic(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The sherpa-onnx release archive `stem` (`.tar.bz2`) at `release`, downloaded, checked and
/// unpacked once into the model cache.
pub(crate) fn unpacked(release: &str, stem: &str, sha256: &str) -> Result<PathBuf> {
    let root = cache_dir().join("sherpa-onnx");
    let dir = root.join(stem);
    // Written last, so a half-unpacked directory is not taken for the model.
    let done = dir.join(".unpacked");
    if done.exists() {
        return Ok(dir);
    }
    let file = format!("{stem}.tar.bz2");
    let archive = fetch_from(&root, release, (&file, sha256))?;
    let reader = std::fs::File::open(&archive).with_context(|| format!("opening {}", archive.display()))?;
    tar::Archive::new(bzip2::read::BzDecoder::new(reader))
        .unpack(&root)
        .with_context(|| format!("unpacking {}", archive.display()))?;
    std::fs::write(&done, b"")?;
    // The archive is only needed until it is unpacked; the models volume is small.
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(dir.join("test_wavs"));
    Ok(dir)
}

/// A Hugging Face model repo, with its name for the errors.
pub struct HubRepo {
    name: String,
    api: ApiRepo,
}

impl HubRepo {
    /// `name` at its main revision.
    pub fn model(name: &str) -> Result<Self> {
        Self::at(name, None)
    }

    /// `name` at `revision`, or at its main revision without one.
    pub fn at(name: &str, revision: Option<&str>) -> Result<Self> {
        let api = ApiBuilder::from_env().build()?;
        let repo = match revision {
            Some(revision) => Repo::with_revision(name.to_owned(), RepoType::Model, revision.to_owned()),
            None => Repo::model(name.to_owned()),
        };
        Ok(Self { name: name.to_owned(), api: api.repo(repo) })
    }

    /// The local path of `file`, downloaded on first use.
    pub fn get(&self, file: &str) -> Result<PathBuf> {
        self.api.get(file).with_context(|| format!("fetching {}/{file}", self.name))
    }

    /// Every file in the repo, by its path in it.
    pub fn files(&self) -> Result<Vec<String>> {
        let info = self.api.info().with_context(|| format!("listing {}", self.name))?;
        Ok(info.siblings.into_iter().map(|s| s.rfilename).collect())
    }
}

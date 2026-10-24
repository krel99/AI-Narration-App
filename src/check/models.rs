//! Whisper models (ggml files) for the take check: the offered catalog, where
//! they live (`<library>/models/`), checksum verification and the download.
//!
//! Sizes, memory and speed figures come from the research (measured with
//! random-weight models of the real shapes on a 4-core Xeon); `≈` marks
//! estimates. The SHA-1 values are the ones published in whisper.cpp's
//! models/README.md. Quantised files have no published hash, so their
//! SHA-256 is read from Hugging Face's file listing at download time.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha1::Digest;

use crate::db::Library;

/// Hugging Face host; `<base>/ggerganov/whisper.cpp/resolve/main/<file>`.
pub const HF_BASE: &str = "https://huggingface.co";
const HF_REPO: &str = "ggerganov/whisper.cpp";

pub struct ModelInfo {
    /// whisper.cpp's model name, e.g. `small`.
    pub name: &'static str,
    pub label: &'static str,
    /// Download size in MiB (`approx` = not a published figure).
    pub size_mib: u32,
    pub size_approx: bool,
    /// Peak memory while transcribing, MiB.
    pub ram_mib: u32,
    pub ram_approx: bool,
    /// Seconds per take with 4 threads: every clip shorter than 30 s pays one
    /// full 30 s encoder pass, plus a little decoding.
    pub secs_per_take: f32,
    /// Published SHA-1 (whisper.cpp models/README.md), when there is one.
    pub sha1: Option<&'static str>,
    pub note: &'static str,
}

impl ModelInfo {
    pub fn file_name(&self) -> String {
        format!("ggml-{}.bin", self.name)
    }

    pub fn url(&self, base: &str) -> String {
        format!("{base}/{HF_REPO}/resolve/main/{}", self.file_name())
    }

    /// "466 MiB download · ~780 MB RAM · ~5 s per take (4 threads)"
    pub fn hint(&self) -> String {
        let a = |approx: bool| if approx { "≈" } else { "" };
        format!(
            "{}{} MiB download · {}{} MiB RAM · ~{:.0} s per take with 4 threads",
            a(self.size_approx),
            self.size_mib,
            a(self.ram_approx),
            self.ram_mib,
            self.secs_per_take
        )
    }
}

pub const DEFAULT_MODEL: &str = "small";

/// The offered models, smallest first. All are multilingual (projects may be
/// Czech); `.en` models would refuse other languages.
pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        name: "base",
        label: "Base",
        size_mib: 142,
        size_approx: false,
        ram_mib: 340,
        ram_approx: false,
        secs_per_take: 1.6,
        sha1: Some("465707469ff3a37a2b9b8d8f89f2f99de7299dac"),
        note: "fastest, least accurate; fine for English on slow machines",
    },
    ModelInfo {
        name: "small",
        label: "Small",
        size_mib: 466,
        size_approx: false,
        ram_mib: 780,
        ram_approx: false,
        secs_per_take: 5.2,
        sha1: Some("55356645c2b361a969dfd0ef2c5a50d530afd8d5"),
        note: "default: good accuracy at a moderate cost",
    },
    ModelInfo {
        name: "small-q8_0",
        label: "Small, 8-bit",
        size_mib: 252,
        size_approx: true,
        ram_mib: 565,
        ram_approx: false,
        secs_per_take: 4.5,
        sha1: None,
        note: "as small, less memory and a little faster (no published checksum)",
    },
    ModelInfo {
        name: "large-v3-turbo-q5_0",
        label: "Large v3 Turbo, 5-bit",
        size_mib: 547,
        size_approx: false,
        ram_mib: 965,
        ram_approx: false,
        secs_per_take: 26.5,
        sha1: Some("e050f7970618a659205450ad97eb95a18d69c9ee"),
        note: "best for Czech and other languages; about 5x slower than small",
    },
    ModelInfo {
        name: "large-v3-turbo",
        label: "Large v3 Turbo",
        size_mib: 1536,
        size_approx: false,
        ram_mib: 2000,
        ram_approx: true,
        secs_per_take: 27.0,
        sha1: Some("4af2b29d7ec73d781377bfd1758ca957a807e941"),
        note: "full precision large model; needs the most memory",
    },
];

pub fn info(name: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.name == name)
}

pub fn models_dir(lib: &Library) -> PathBuf {
    lib.root.join("models")
}

pub fn model_path(lib: &Library, m: &ModelInfo) -> PathBuf {
    models_dir(lib).join(m.file_name())
}

/// Written next to a model once its checksum is verified.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Verified {
    size: u64,
    sha1: String,
    sha256: String,
    /// Which published value it was checked against: sha1 | sha256 | none.
    checked_with: String,
}

fn verified_path(model: &Path) -> PathBuf {
    let mut s = model.as_os_str().to_owned();
    s.push(".verified");
    PathBuf::from(s)
}

fn part_path(model: &Path) -> PathBuf {
    let mut s = model.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn read_verified(model: &Path) -> Option<Verified> {
    let v: Verified = serde_json::from_slice(&std::fs::read(verified_path(model)).ok()?).ok()?;
    let size = std::fs::metadata(model).ok()?.len();
    (v.size == size).then_some(v)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    No,
    /// Downloaded and checked against a published checksum.
    Verified,
    /// Present, but not (yet) checked: placed by hand, or no checksum is
    /// published for it.
    Unverified,
}

pub fn installed(lib: &Library, m: &ModelInfo) -> Installed {
    let path = model_path(lib, m);
    if !path.is_file() {
        return Installed::No;
    }
    match read_verified(&path) {
        Some(v) if v.checked_with != "none" => Installed::Verified,
        _ => Installed::Unverified,
    }
}

/// Before loading: a file with a published SHA-1 that was not verified yet
/// (placed by hand, or changed since) is hashed once. A wrong or bit-rotted
/// file would otherwise load and transcribe garbage.
pub fn verify_before_load(path: &Path, m: &ModelInfo) -> Result<()> {
    if read_verified(path).is_some() {
        return Ok(());
    }
    let Some(expected) = m.sha1 else {
        log::warn!(
            "{}: no published checksum, loading it unverified",
            path.display()
        );
        return Ok(());
    };
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let (mut h1, mut h256) = (sha1::Sha1::new(), sha2::Sha256::new());
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h1.update(&buf[..n]);
        h256.update(&buf[..n]);
        size += n as u64;
    }
    let sha1 = hex::encode(h1.finalize());
    if sha1 != expected {
        bail!(
            "{} does not match the published checksum; delete it and download it again",
            path.display()
        );
    }
    write_verified(
        path,
        &Verified {
            size,
            sha1,
            sha256: hex::encode(h256.finalize()),
            checked_with: "sha1".into(),
        },
    );
    Ok(())
}

fn write_verified(path: &Path, v: &Verified) {
    if let Err(e) = std::fs::write(
        verified_path(path),
        serde_json::to_vec_pretty(v).expect("serializable"),
    ) {
        log::warn!("{}: {e}", verified_path(path).display());
    }
}

/// Delete a model and its bookkeeping files.
pub fn delete(lib: &Library, m: &ModelInfo) -> Result<()> {
    let path = model_path(lib, m);
    for p in [verified_path(&path), part_path(&path)] {
        let _ = std::fs::remove_file(p);
    }
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow!("deleting {}: {e}", path.display())),
    }
}

/// The SHA-256 and size of a file from Hugging Face's tree listing
/// (`lfs.oid` is the SHA-256 of an LFS file). Best effort: `None` when the
/// listing cannot be read.
async fn hf_sha256(http: &reqwest::Client, base: &str, file: &str) -> Option<(String, u64)> {
    let url = format!("{base}/api/models/{HF_REPO}/tree/main");
    let resp = http
        .get(&url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let entries: Vec<serde_json::Value> = resp.json().await.ok()?;
    let e = entries.iter().find(|e| e["path"].as_str() == Some(file))?;
    let oid = e["lfs"]["oid"].as_str()?.to_ascii_lowercase();
    let size = e["lfs"]["size"].as_u64().or_else(|| e["size"].as_u64())?;
    (oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_hexdigit())).then_some((oid, size))
}

/// A download gives up after this long without any data.
const STALL: Duration = Duration::from_secs(60);

/// Await `fut`, but give up when `cancel` is set or after `stall` (the
/// connection can hang without ever delivering another chunk, and the client
/// has no timeout of its own: model files take minutes).
async fn guarded<T>(
    fut: impl std::future::Future<Output = T>,
    cancel: &AtomicBool,
    stall: Duration,
) -> Result<T> {
    tokio::pin!(fut);
    let deadline = Instant::now() + stall;
    loop {
        tokio::select! {
            r = &mut fut => return Ok(r),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if cancel.load(Ordering::Relaxed) {
                    bail!("download cancelled");
                }
                if Instant::now() >= deadline {
                    bail!("download stalled: no data for {} s", stall.as_secs());
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Downloaded {
    pub path: PathBuf,
    pub bytes: u64,
    /// Checked against a published SHA-1 or Hugging Face's SHA-256.
    pub verified: bool,
}

/// Stream a model to `<file>.part`, verify it, then rename it into place.
/// `progress(received, total)` is called at most a few times per second.
/// Setting `cancel` stops the download and removes the partial file.
pub async fn download(
    http: &reqwest::Client,
    base: &str,
    lib: &Library,
    m: &ModelInfo,
    cancel: &AtomicBool,
    progress: impl Fn(u64, Option<u64>),
) -> Result<Downloaded> {
    download_with_stall(http, base, lib, m, cancel, progress, STALL).await
}

async fn download_with_stall(
    http: &reqwest::Client,
    base: &str,
    lib: &Library,
    m: &ModelInfo,
    cancel: &AtomicBool,
    progress: impl Fn(u64, Option<u64>),
    stall: Duration,
) -> Result<Downloaded> {
    let dir = models_dir(lib);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = model_path(lib, m);
    let part = part_path(&path);
    let listed = if m.sha1.is_none() {
        hf_sha256(http, base, &m.file_name()).await
    } else {
        None
    };
    let mut req = http.get(m.url(base));
    if let Ok(token) = std::env::var("HF_TOKEN")
        && !token.trim().is_empty()
    {
        req = req.bearer_auth(token.trim());
    }
    let mut resp = guarded(req.send(), cancel, stall)
        .await?
        .with_context(|| format!("downloading {}", m.file_name()))?;
    if !resp.status().is_success() {
        bail!("downloading {}: HTTP {}", m.url(base), resp.status());
    }
    let total = resp.content_length().or(listed.as_ref().map(|l| l.1));
    let result = async {
        let mut file =
            std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
        let (mut h1, mut h256) = (sha1::Sha1::new(), sha2::Sha256::new());
        let mut received = 0u64;
        let mut last = Instant::now();
        progress(0, total);
        while let Some(chunk) = guarded(resp.chunk(), cancel, stall)
            .await?
            .context("download interrupted")?
        {
            if cancel.load(Ordering::Relaxed) {
                bail!("download cancelled");
            }
            file.write_all(&chunk)?;
            h1.update(&chunk);
            h256.update(&chunk);
            received += chunk.len() as u64;
            if last.elapsed() >= Duration::from_millis(200) {
                last = Instant::now();
                progress(received, total);
            }
        }
        file.sync_all()?;
        progress(received, total);
        if let Some(t) = total
            && t != received
        {
            bail!("download incomplete: {received} of {t} bytes");
        }
        let sha1 = hex::encode(h1.finalize());
        let sha256 = hex::encode(h256.finalize());
        let checked_with = if let Some(expected) = m.sha1 {
            if sha1 != expected {
                bail!("the downloaded file does not match the published SHA-1 checksum");
            }
            "sha1"
        } else if let Some((expected, _)) = &listed {
            if &sha256 != expected {
                bail!("the downloaded file does not match Hugging Face's SHA-256 checksum");
            }
            "sha256"
        } else {
            log::warn!("{}: no checksum available; kept unverified", m.file_name());
            "none"
        };
        Ok(Verified {
            size: received,
            sha1,
            sha256,
            checked_with: checked_with.into(),
        })
    }
    .await;
    let verified = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    std::fs::rename(&part, &path)
        .with_context(|| format!("moving {} into place", part.display()))?;
    write_verified(&path, &verified);
    Ok(Downloaded {
        path,
        bytes: verified.size,
        verified: verified.checked_with != "none",
    })
}

#[cfg(test)]
pub(crate) fn mark_verified_for_tests(path: &Path) {
    let size = std::fs::metadata(path).unwrap().len();
    write_verified(
        path,
        &Verified {
            size,
            sha1: String::new(),
            sha256: String::new(),
            checked_with: "sha1".into(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent() {
        assert!(info(DEFAULT_MODEL).is_some());
        let mut names: Vec<&str> = MODELS.iter().map(|m| m.name).collect();
        names.dedup();
        assert_eq!(names.len(), MODELS.len());
        for m in MODELS {
            assert!(!m.name.ends_with(".en"), "{}: English-only", m.name);
            if let Some(h) = m.sha1 {
                assert!(
                    h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{}",
                    m.name
                );
            }
        }
        assert_eq!(
            info("small").unwrap().url(HF_BASE),
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin"
        );
        assert!(info("small").unwrap().hint().contains("466 MiB download"));
        assert!(info("small-q8_0").unwrap().hint().starts_with("≈252 MiB"));
    }

    /// A server that sends the headers and a few bytes, then hangs.
    fn hanging_server() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let mut s = stream.unwrap();
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ =
                    s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1000000\r\n\r\n0123456789");
                let _ = s.flush();
                std::thread::sleep(Duration::from_secs(10));
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn a_hanging_download_can_be_cancelled_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let m = info("base").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let base = hanging_server();
        // Cancel while no data arrives.
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            c2.store(true, Ordering::SeqCst);
        });
        let t = Instant::now();
        let e = rt
            .block_on(download_with_stall(
                &http,
                &base,
                &lib,
                m,
                &cancel,
                |_, _| {},
                Duration::from_secs(30),
            ))
            .unwrap_err();
        assert!(format!("{e:#}").contains("cancelled"), "{e:#}");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        // Nothing arrives for the stall time: it gives up.
        let never = AtomicBool::new(false);
        let e = rt
            .block_on(download_with_stall(
                &http,
                &base,
                &lib,
                m,
                &never,
                |_, _| {},
                Duration::from_millis(400),
            ))
            .unwrap_err();
        assert!(format!("{e:#}").contains("stalled"), "{e:#}");
        assert_eq!(installed(&lib, m), Installed::No);
        assert!(!part_path(&model_path(&lib, m)).exists(), "no .part left");
    }

    #[test]
    fn verification_before_load() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().to_path_buf());
        let m = info("base").unwrap();
        let path = model_path(&lib, m);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not a model").unwrap();
        assert_eq!(installed(&lib, m), Installed::Unverified);
        // A file that does not match the published SHA-1 is refused.
        let e = verify_before_load(&path, m).unwrap_err().to_string();
        assert!(e.contains("does not match the published checksum"), "{e}");
        // A verified file loads without hashing; changing it voids that.
        mark_verified_for_tests(&path);
        assert_eq!(installed(&lib, m), Installed::Verified);
        verify_before_load(&path, m).unwrap();
        std::fs::write(&path, b"not a model either").unwrap();
        assert_eq!(installed(&lib, m), Installed::Unverified);
        assert!(verify_before_load(&path, m).is_err());
        // No published hash: loads unverified.
        let q = info("small-q8_0").unwrap();
        std::fs::write(model_path(&lib, q), b"x").unwrap();
        verify_before_load(&model_path(&lib, q), q).unwrap();
        delete(&lib, m).unwrap();
        delete(&lib, m).unwrap();
        assert_eq!(installed(&lib, m), Installed::No);
    }
}

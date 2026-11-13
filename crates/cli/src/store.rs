//! Where a published release's files live: a Hugging Face dataset,
//! [`REPO`], one archive per version (`<version>/release.tar`). The repo
//! keeps only the release's `release.json`, which names the dataset
//! revision and the archive's sha256 (`"store"`) and holds the release's
//! scores. Readers fetch the archive once, check it, and unpack it into
//! `~/.cache/mtx/results/`.
//!
//! One archive, not the release's thousands of files: the Hub limits how
//! many files a reader may fetch in a few minutes, and a release's files
//! are gzipped already, so the archive isn't compressed again.
//!
//! Uploading shells out to the `hf` CLI (`uv tool install huggingface_hub`),
//! with `HF_TOKEN`. Fetching needs neither: the dataset is public.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The dataset releases are uploaded to; `MTX_RESULTS_REPO` overrides it.
pub const REPO: &str = "solana-foundation/machine-trading-results";
const HUB: &str = "https://huggingface.co";
/// A release's archive, in its version's directory.
const ARCHIVE: &str = "release.tar";
/// The marker a fully unpacked release's cache holds.
pub const COMPLETE: &str = ".complete";

pub fn repo() -> String {
    std::env::var("MTX_RESULTS_REPO").unwrap_or_else(|_| REPO.to_string())
}

/// A release's `"store"`: where its archive is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    pub repo: String,
    pub revision: String,
    /// The archive's path in the dataset: `<version>/release.tar`.
    pub path: String,
    pub sha256: String,
    /// The archive's size.
    pub bytes: u64,
    /// How many files it holds.
    pub files: usize,
}

impl Store {
    pub fn of(release: &Value) -> Option<Store> {
        let store = &release["store"];
        Some(Store {
            repo: store["repo"].as_str()?.to_string(),
            revision: store["revision"].as_str()?.to_string(),
            path: store["path"].as_str()?.to_string(),
            sha256: store["sha256"].as_str()?.to_string(),
            bytes: store["bytes"].as_u64()?,
            files: store["files"].as_u64().unwrap_or(0) as usize,
        })
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "repo": self.repo,
            "revision": self.revision,
            "path": self.path,
            "sha256": self.sha256,
            "bytes": self.bytes,
            "files": self.files,
        })
    }

    /// The archive's page on the Hub.
    pub fn url(&self) -> String {
        format!(
            "{HUB}/datasets/{}/blob/{}/{}",
            self.repo, self.revision, self.path
        )
    }

    fn download_url(&self) -> String {
        format!(
            "{HUB}/datasets/{}/resolve/{}/{}",
            self.repo, self.revision, self.path
        )
    }

    /// Where its files are unpacked to.
    pub fn cache(&self) -> PathBuf {
        let version = self.path.split('/').next().unwrap_or(&self.path);
        cache_root()
            .join(self.repo.replace('/', "--"))
            .join(format!(
                "{version}@{}",
                &self.sha256[..self.sha256.len().min(12)]
            ))
    }
}

fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("MTX_CACHE") {
        return PathBuf::from(dir).join("results");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".cache/mtx/results")
}

/// Pack `dir` (a whole release) and upload it as `<version>/release.tar`,
/// replacing whatever the version held; then check the dataset holds that
/// archive at the new revision.
pub fn upload(dir: &Path, version: &str) -> Result<Store> {
    let repo = repo();
    ensure!(
        std::env::var_os("HF_TOKEN").is_some(),
        "HF_TOKEN isn't set: uploading to {repo} needs a write token"
    );
    let scratch =
        std::env::temp_dir().join(format!("mtx-release-{version}-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let archive = scratch.join(ARCHIVE);
    let files = pack(dir, &archive)?;
    let bytes = std::fs::metadata(&archive)?.len();
    let sha256 = sha256_file(&archive)?;
    let path = format!("{version}/{ARCHIVE}");
    let status = std::process::Command::new("hf")
        .args(["upload", &repo])
        .arg(&archive)
        .arg(&path)
        .args(["--repo-type", "dataset", "--quiet"])
        .args(["--commit-message", &format!("Release {version}")])
        .status()
        .context("running hf: install it with `uv tool install huggingface_hub`")?;
    let _ = std::fs::remove_dir_all(&scratch);
    ensure!(status.success(), "hf upload failed ({status})");
    // Whatever else the version held, an earlier upload's, goes. (`hf
    // upload --delete` skips its commit when the archive is unchanged.)
    let stale: Vec<String> = run(async {
        let revision = head(&repo).await?;
        list(&repo, &revision, version).await
    })?
    .into_iter()
    .filter(|e| e["path"] != serde_json::json!(path))
    .filter_map(|e| {
        let at = e["path"].as_str()?;
        Some(if e["type"] == serde_json::json!("directory") {
            format!("{at}/*")
        } else {
            at.to_string()
        })
    })
    .collect();
    if !stale.is_empty() {
        let status = std::process::Command::new("hf")
            .args(["repos", "delete-files", &repo])
            .args(&stale)
            .args(["--repo-type", "dataset", "--quiet"])
            .args([
                "--commit-message",
                &format!("Release {version}: only its archive"),
            ])
            .status()?;
        ensure!(status.success(), "hf repos delete-files failed ({status})");
    }
    let (revision, entries) = run(async {
        let revision = head(&repo).await?;
        let entries = list(&repo, &revision, version).await?;
        Ok((revision, entries))
    })?;
    // The version's directory holds the archive alone, the one just packed.
    let found: Vec<String> = entries.iter().map(|e| e["path"].to_string()).collect();
    ensure!(
        entries.len() == 1
            && entries[0]["path"] == serde_json::json!(path)
            && entries[0]["lfs"]["oid"].as_str() == Some(sha256.as_str())
            && entries[0]["size"].as_u64() == Some(bytes),
        "{repo}@{revision} doesn't hold the archive just uploaded under {version}/: {}",
        found.join(", ")
    );
    Ok(Store {
        repo,
        revision,
        path,
        sha256,
        bytes,
        files,
    })
}

/// The store's files, fetched and unpacked once into its cache; the
/// cache's path.
pub fn fetch(store: &Store) -> Result<PathBuf> {
    let dir = store.cache();
    if dir.join(COMPLETE).is_file() {
        return Ok(dir);
    }
    let parent = dir.parent().context("the cache has no parent")?;
    std::fs::create_dir_all(parent)?;
    let archive = dir.with_extension("tar.partial");
    run(download(store, &archive))?;
    let sha256 = sha256_file(&archive)?;
    if sha256 != store.sha256 {
        let _ = std::fs::remove_file(&archive);
        bail!(
            "{}: sha256 {sha256}, the release says {}",
            store.url(),
            store.sha256
        );
    }
    let staging = dir.with_extension("partial");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    // `unpack` keeps every entry under `staging`: no `..`, no absolute path.
    tar::Archive::new(std::fs::File::open(&archive)?)
        .unpack(&staging)
        .with_context(|| format!("unpacking {}", archive.display()))?;
    std::fs::remove_file(&archive)?;
    std::fs::write(staging.join(COMPLETE), "")?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::rename(&staging, &dir)?;
    Ok(dir)
}

/// Keep `dir`, the release just uploaded, as the store's cache: nothing to
/// fetch for what is already here.
pub fn adopt(store: &Store, dir: &Path) -> Result<PathBuf> {
    let cache = store.cache();
    if cache.exists() {
        std::fs::remove_dir_all(&cache)?;
    }
    std::fs::create_dir_all(cache.parent().context("the cache has no parent")?)?;
    if std::fs::rename(dir, &cache).is_err() {
        // Another filesystem: copy it.
        copy_dir(dir, &cache)?;
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::write(cache.join(COMPLETE), "")?;
    Ok(cache)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// Every file under `dir`, relative to it, in order.
fn files_of(dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(root, &entry.path(), out)?;
            } else if kind.is_file() {
                let rel = entry.path().strip_prefix(root)?.to_path_buf();
                if rel != Path::new(COMPLETE) {
                    out.push(rel);
                }
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// `dir`'s files as a tar at `to`, the same bytes for the same files: in
/// order, with no owner, mode or time of this machine's. How many it holds.
fn pack(dir: &Path, to: &Path) -> Result<usize> {
    let files = files_of(dir)?;
    let mut builder = tar::Builder::new(std::io::BufWriter::new(std::fs::File::create(to)?));
    builder.mode(tar::HeaderMode::Deterministic);
    for rel in &files {
        builder.append_path_with_name(dir.join(rel), rel)?;
    }
    builder.into_inner()?.flush()?;
    Ok(files.len())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut std::fs::File::open(path)?, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Run `future` on a runtime of its own, on a thread of its own: callers may
/// already be inside one.
fn run<T: Send>(future: impl std::future::Future<Output = Result<T>> + Send) -> Result<T> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(future)
            })
            .join()
            .map_err(|_| anyhow::anyhow!("the fetch thread panicked"))?
    })
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("mtx/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// The dataset's main revision.
async fn head(repo: &str) -> Result<String> {
    let url = format!("{HUB}/api/datasets/{repo}/revision/main");
    let info: Value = client()?
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    info["sha"]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("{url} names no revision"))
}

/// The files and directories directly under `path` at `revision`, as the
/// Hub lists them.
async fn list(repo: &str, revision: &str, path: &str) -> Result<Vec<Value>> {
    let url = format!("{HUB}/api/datasets/{repo}/tree/{revision}/{path}");
    let entries: Vec<Value> = client()?
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| url.clone())?
        .json()
        .await?;
    Ok(entries)
}

/// The archive, streamed to `to`.
async fn download(store: &Store, to: &Path) -> Result<()> {
    let url = store.download_url();
    let mut response = client()?
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| url.clone())?;
    let mut file = std::io::BufWriter::new(std::fs::File::create(to)?);
    let mut written = 0u64;
    while let Some(chunk) = response.chunk().await? {
        written += chunk.len() as u64;
        file.write_all(&chunk)?;
    }
    file.flush()?;
    ensure!(
        written == store.bytes,
        "{url}: {written} bytes, the release says {}",
        store.bytes
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_is_read_from_its_release() {
        let release = serde_json::json!({ "store": {
            "repo": "solana-foundation/machine-trading-results",
            "revision": "d9f88100ed03aafcad6385b0f43fc660416ee3bb",
            "path": "0.12.0/release.tar",
            "sha256": "0123456789abcdef0123",
            "bytes": 85_000_000,
            "files": 7079,
        }});
        let store = Store::of(&release).unwrap();
        assert_eq!(
            Store::of(&serde_json::json!({ "store": store.to_json() })),
            Some(store.clone())
        );
        assert!(
            store
                .cache()
                .ends_with("solana-foundation--machine-trading-results/0.12.0@0123456789ab")
        );
        assert_eq!(Store::of(&serde_json::json!({ "version": "0.11.0" })), None);
    }

    #[test]
    fn an_archive_is_the_same_bytes_for_the_same_files_and_unpacks_to_them() {
        let root = std::env::temp_dir().join(format!("mtx-store-test-{}", std::process::id()));
        let dir = root.join("release");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("release.json"), "{}").unwrap();
        std::fs::write(dir.join("a/b/session.json.gz"), [1u8, 2, 3]).unwrap();
        std::fs::write(dir.join(COMPLETE), "").unwrap();
        let (one, two) = (root.join("one.tar"), root.join("two.tar"));
        assert_eq!(pack(&dir, &one).unwrap(), 2);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dir.join("release.json"), "{}").unwrap();
        pack(&dir, &two).unwrap();
        assert_eq!(sha256_file(&one).unwrap(), sha256_file(&two).unwrap());
        let out = root.join("out");
        tar::Archive::new(std::fs::File::open(&one).unwrap())
            .unpack(&out)
            .unwrap();
        assert_eq!(files_of(&out).unwrap(), files_of(&dir).unwrap());
        assert_eq!(
            std::fs::read(out.join("a/b/session.json.gz")).unwrap(),
            [1, 2, 3]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

//! Untrusted artifact inspection. Never runs candidate code in the grader.
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use crate::fixture::hash_file;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Program {
    pub address: String,
    pub file: String,
    pub sha256: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scratch {
    pub address: String,
    pub owner: String,
    pub bytes: usize,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub executable: String,
    #[serde(default)]
    pub executor: String,
    pub programs: Vec<Program>,
    pub scratch: Vec<Scratch>,
}

/// Single-component, regular files only. No symlink, hardlink, device, path
/// traversal, or recursive artifact trees are accepted by the trusted loader.
pub fn file(root: &Path, name: &str, maximum: u64) -> Result<PathBuf> {
    ensure!(
        !name.is_empty() && Path::new(name).components().count() == 1 && !name.starts_with('.'),
        "invalid artifact filename"
    );
    let path = root.join(name);
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= maximum,
        "invalid artifact file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(metadata.nlink() == 1, "hardlinked artifact");
    }
    Ok(path)
}

pub fn inspect(root: &Path, reserved: &BTreeSet<Pubkey>) -> Result<(Manifest, String)> {
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(file(root, "manifest.json", 32_768)?)?)?;
    ensure!(
        manifest.version == 2 && manifest.programs.len() <= 4,
        "unsupported bot manifest"
    );
    file(root, "run.sh", 16_384)?;
    let native = fs::read(file(root, &manifest.executable, 128 * 1024 * 1024)?)?;
    ensure!(
        native.len() >= 64 && native.starts_with(b"\x7fELF") && native[4] == 2 && native[5] == 1,
        "native executable must be a 64-bit little-endian ELF"
    );
    let machine = u16::from_le_bytes(native[18..20].try_into()?);
    ensure!(
        machine
            == if cfg!(target_arch = "aarch64") {
                183
            } else {
                62
            },
        "native executable architecture mismatch"
    );
    ensure!(
        manifest.executable != "run.sh" && manifest.executable != "manifest.json",
        "duplicate artifact names"
    );
    let mut addresses = BTreeSet::new();
    let mut files = BTreeSet::from([
        "manifest.json".to_string(),
        "run.sh".to_string(),
        manifest.executable.clone(),
    ]);
    let mut total = 0;
    for program in &manifest.programs {
        let address: Pubkey = program.address.parse()?;
        ensure!(
            !reserved.contains(&address) && addresses.insert(address),
            "reserved or duplicate program identifier"
        );
        ensure!(files.insert(program.file.clone()), "duplicate program file");
        let path = file(root, &program.file, 2 * 1024 * 1024)?;
        let bytes = fs::read(&path)?;
        ensure!(
            bytes.len() >= 64
                && bytes.starts_with(b"\x7fELF")
                && u16::from_le_bytes(bytes[18..20].try_into()?) == 247
                && hash_file(&path)? == program.sha256,
            "invalid SBF artifact or digest"
        );
    }
    ensure!(
        manifest.executor.is_empty() || addresses.contains(&manifest.executor.parse()?),
        "executor must be a declared program"
    );
    ensure!(manifest.scratch.len() <= 16, "too many scratch accounts");
    let programs = addresses.clone();
    // The upgradeable loader owns a second derived account for every program.
    // Reserve those too: a declared program/scratch address must never replace
    // another program's bytecode account during trusted installation.
    for program in &programs {
        let data = program_data_address(program);
        ensure!(
            !reserved.contains(&data) && addresses.insert(data),
            "program-data address collision"
        );
    }
    for scratch in &manifest.scratch {
        let address: Pubkey = scratch.address.parse()?;
        ensure!(
            !reserved.contains(&address) && addresses.insert(address),
            "reserved or duplicate scratch address"
        );
        ensure!(
            programs.contains(&scratch.owner.parse()?) && scratch.bytes <= 16_384,
            "invalid scratch account"
        );
        total += scratch.bytes;
    }
    ensure!(total <= 65_536, "scratch allocation exceeds 64 KiB");
    let names = files.iter().map(String::as_str).collect::<Vec<_>>();
    let hash = crate::fixture::seal(root, &names)?.fixture_hash;
    Ok((manifest, hash))
}

pub fn program_data_address(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[program.as_ref()],
        &crate::programs::key("BPFLoaderUpgradeab1e11111111111111111111111"),
    )
    .0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_and_symlinks_are_not_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bot"), b"native").unwrap();
        assert!(file(dir.path(), "bot", 6).is_ok());
        assert!(file(dir.path(), "bot", 5).is_err());
        assert!(file(dir.path(), "../bot", 6).is_err());
        assert!(file(dir.path(), "/etc/passwd", 10000).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("bot", dir.path().join("alias")).unwrap();
            assert!(file(dir.path(), "alias", 6).is_err());
        }
    }
}

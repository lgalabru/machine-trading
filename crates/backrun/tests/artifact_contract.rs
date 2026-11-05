//! Parser/firewall tests do not execute these synthetic ELF headers.
use std::{collections::BTreeSet, fs};

use mth_backrun::{artifact, fixture::hash_file};
use solana_pubkey::Pubkey;

fn artifact() -> (tempfile::TempDir, serde_json::Value, Pubkey) {
    let dir = tempfile::tempdir().unwrap();
    let mut elf = vec![0; 64];
    elf[..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2;
    elf[5] = 1;
    let native: u16 = if cfg!(target_arch = "aarch64") {
        183
    } else {
        62
    };
    elf[18..20].copy_from_slice(&native.to_le_bytes());
    fs::write(dir.path().join("bot"), &elf).unwrap();
    fs::write(dir.path().join("run.sh"), "#!/bin/sh\nexec ./bot\n").unwrap();
    elf[18..20].copy_from_slice(&247u16.to_le_bytes());
    fs::write(dir.path().join("executor.so"), &elf).unwrap();
    let id = Pubkey::new_unique();
    let manifest = serde_json::json!({"version":2,"executable":"bot","executor":id.to_string(),"programs":[{"address":id.to_string(),"file":"executor.so","sha256":hash_file(&dir.path().join("executor.so")).unwrap()}],"scratch":[]});
    (dir, manifest, id)
}
fn write(dir: &tempfile::TempDir, value: &serde_json::Value) {
    fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(value).unwrap(),
    )
    .unwrap();
}
#[test]
fn loader_accounts_and_scratch_cannot_alias_or_replace_trusted_programs() {
    let (dir, mut manifest, id) = artifact();
    write(&dir, &manifest);
    let data = artifact::program_data_address(&id);
    assert!(artifact::inspect(dir.path(), &BTreeSet::new()).is_ok());
    assert!(artifact::inspect(dir.path(), &BTreeSet::from([id])).is_err());
    assert!(artifact::inspect(dir.path(), &BTreeSet::from([data])).is_err());
    manifest["scratch"] =
        serde_json::json!([{"address":data.to_string(),"owner":id.to_string(),"bytes":8}]);
    write(&dir, &manifest);
    assert!(artifact::inspect(dir.path(), &BTreeSet::new()).is_err());
}
#[test]
fn digest_changes_and_unbounded_or_foreign_scratch_are_rejected() {
    let (dir, mut manifest, id) = artifact();
    manifest["scratch"] = serde_json::json!([{"address":Pubkey::new_unique().to_string(),"owner":id.to_string(),"bytes":16385}]);
    write(&dir, &manifest);
    assert!(artifact::inspect(dir.path(), &BTreeSet::new()).is_err());
    manifest["scratch"][0]["bytes"] = 8.into();
    manifest["scratch"][0]["owner"] = Pubkey::new_unique().to_string().into();
    write(&dir, &manifest);
    assert!(artifact::inspect(dir.path(), &BTreeSet::new()).is_err());
    manifest["scratch"] = serde_json::json!([]);
    write(&dir, &manifest);
    let mut binary = fs::read(dir.path().join("executor.so")).unwrap();
    binary[40] = 1;
    fs::write(dir.path().join("executor.so"), binary).unwrap();
    assert!(artifact::inspect(dir.path(), &BTreeSet::new()).is_err());
}

#[test]
fn native_only_bot_needs_no_executor() {
    let (dir, mut manifest, _) = artifact();
    manifest["programs"] = serde_json::json!([]);
    manifest.as_object_mut().unwrap().remove("executor");
    write(&dir, &manifest);
    assert!(mth_backrun::artifact::inspect(dir.path(), &std::collections::BTreeSet::new()).is_ok());
}

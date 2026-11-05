#!/usr/bin/env python3
"""Export only pool/playground SDK code. No grader, rivals or calibration bots."""
import pathlib, shutil, sys
repo, out = map(pathlib.Path, sys.argv[1:])
out.mkdir(parents=True, exist_ok=False)
root = (repo/'Cargo.toml').read_text().replace('members = ["crates/*", "bots/*"]', 'members = ["crates/backrun"]').replace('default-members = ["crates/cli"]', 'default-members = ["crates/backrun"]')
(out/'Cargo.toml').write_text(root)
for file in ['Cargo.lock','rust-toolchain']:
    shutil.copyfile(repo/file,out/file)
crate=out/'crates/backrun'; (crate/'src/bin').mkdir(parents=True)
shutil.copyfile(repo/'crates/backrun/Cargo.toml',crate/'Cargo.toml')
modules=['fixture','wallet','programs','sdk','world','playground']
for module in modules:
    text=(repo/f'crates/backrun/src/{module}.rs').read_text()
    if module=='playground': text=text.replace('runner::REFERENCE_EXECUTOR,','REFERENCE_EXECUTOR,')
    (crate/f'src/{module}.rs').write_text(text)
constants=(repo/'crates/backrun/src/lib.rs').read_text().split('pub const LAMPORTS_PER_SOL',1)[1]
(crate/'src/lib.rs').write_text('//! Public, unscored pool SDK.\n'+''.join(f'pub mod {m};\n' for m in modules)+'pub const LAMPORTS_PER_SOL'+constants+'\npub const REFERENCE_EXECUTOR: solana_pubkey::Pubkey = solana_pubkey::Pubkey::new_from_array([69;32]);\n')
shutil.copyfile(repo/'bench/backrun/playground-main.rs',crate/'src/bin/mth-backrun.rs')
assert not any((crate/f'src/{m}.rs').exists() for m in ['reference','runner','build','cli'])

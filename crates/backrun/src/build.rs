use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, ensure};
use rand::{Rng, SeedableRng, rngs::StdRng};
use solana_keypair::{Keypair, read_keypair_file};
use solana_signer::Signer;

use crate::{
    WSOL,
    fixture::{self, Leg, Market, PrivateFlow, UserOrder},
    programs::*,
    world::World,
};

pub async fn build(
    programs: &Path,
    cpmm_admin: &Path,
    orca_admin: &Path,
    out: &Path,
) -> Result<String> {
    ensure!(
        !out.exists(),
        "output already exists; immutable fixtures are never overwritten"
    );
    let parent = out
        .parent()
        .context("fixture requires a parent directory")?;
    fs::create_dir_all(parent)?;
    // Stage privately and publish with a rename only after complete validation.
    let staging = parent.join(format!(".backrun-build-{}", std::process::id()));
    fs::create_dir(&staging)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    }
    let result = build_into(programs, cpmm_admin, orca_admin, &staging).await;
    match result {
        Ok(hash) => {
            fs::rename(staging, out)?;
            Ok(hash)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(staging);
            Err(e)
        }
    }
}

async fn build_into(
    programs: &Path,
    cpmm_admin: &Path,
    orca_admin: &Path,
    out: &Path,
) -> Result<String> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(programs.join("manifest.json"))?)?;
    ensure!(
        manifest["raydium_revision"] == RAYDIUM_REV
            && manifest["orca_revision"] == ORCA_REV
            && manifest["surfpool_revision"] == SURFPOOL_REV,
        "program provenance mismatch"
    );
    let mut hashes = BTreeMap::new();
    for name in [
        "raydium_cp_swap.so",
        "whirlpool.so",
        "mth_backrun_executor.so",
    ] {
        let hash = fixture::hash_file(&programs.join(name))?;
        ensure!(
            manifest["files"][name] == hash,
            "program hash mismatch: {name}"
        );
        hashes.insert(name.into(), hash);
    }
    let world = World::new(programs, cpmm_admin).await?;
    ensure!(
        manifest["cpmm_localnet_admin"] == world.payer().pubkey().to_string(),
        "CPMM admin does not match program build"
    );
    let orca_admin = read_keypair_file(orca_admin).map_err(|e| anyhow::anyhow!("{e}"))?;
    let seed: [u8; 32] = rand::random();
    let mut rng = StdRng::from_seed(seed);
    let edges = fixture::topology(seed);
    let mut tokens = vec![key(WSOL)];
    for _ in 1..24 {
        tokens.push(world.create_mint().await?);
    }
    let wallets: Vec<Keypair> = (0..8).map(|_| Keypair::new()).collect();
    for mint in &tokens {
        world
            .fund_tokens(world.payer().pubkey(), *mint, 1_000_000_000_000_000)
            .await?;
        for user in &wallets {
            world
                .fund_tokens(user.pubkey(), *mint, 10_000_000_000_000)
                .await?;
        }
    }
    for user in &wallets {
        world
            .surfnet
            .cheatcodes()
            .fund_sol(&user.pubkey(), 100_000_000_000)?;
    }
    let mut cp_configs = Vec::new();
    let mut wp_configs = Vec::new();
    for (i, fee) in [500, 1500, 3000, 5000].into_iter().enumerate() {
        cp_configs.push(world.cpmm_config(i as u16, fee).await?);
        wp_configs.push(world.whirlpool_config(&orca_admin, 8, fee as u16).await?);
    }
    let fees = [500, 1500, 3000, 5000];
    let mut pools = Vec::new();
    for (i, edge) in edges.into_iter().enumerate() {
        let tier = i % 4;
        let depth = rng.random_range(500..=2500) * 1_000_000_000u64;
        let pool = if i < 64 {
            world
                .create_cpmm(cp_configs[tier], &tokens, edge, depth, fees[tier])
                .await?
        } else {
            world
                .create_whirlpool(
                    wp_configs[tier],
                    &tokens,
                    edge,
                    8,
                    fees[tier],
                    u128::from(depth) * 5,
                )
                .await?
        };
        pools.push(pool);
        tracing::info!(pools = i + 1, "initialized actual AMM pool");
    }
    for mint in &tokens {
        world.revoke_mint(*mint).await?;
    }
    // Unsignable PDA authorities retire mutable pool configuration. Raydium's
    // compiled localnet administrator remains a private setup-only capability.
    let retired = pda(key(crate::CPMM), &[b"backrun-retired-authority"]);
    for config in cp_configs {
        for parameter in [3u8, 4] {
            let mut args = vec![parameter];
            args.extend_from_slice(&0u64.to_le_bytes());
            world
                .send(
                    vec![anchor(
                        key(crate::CPMM),
                        "update_amm_config",
                        vec![
                            solana_instruction::AccountMeta::new_readonly(
                                world.payer().pubkey(),
                                true,
                            ),
                            rw(config),
                            ro(retired),
                        ],
                        &args,
                    )],
                    &[],
                )
                .await?;
        }
    }
    for config in wp_configs {
        for instruction in [
            "set_fee_authority",
            "set_collect_protocol_fees_authority",
            "set_reward_emissions_super_authority",
        ] {
            world
                .send(
                    vec![anchor(
                        key(crate::WHIRLPOOL),
                        instruction,
                        vec![
                            rw(config),
                            solana_instruction::AccountMeta::new_readonly(
                                world.payer().pubkey(),
                                true,
                            ),
                            ro(retired),
                        ],
                        &[],
                    )],
                    &[],
                )
                .await?;
        }
    }
    let market = Market {
        version: 1,
        tokens: tokens.iter().map(ToString::to_string).collect(),
        pools,
        program_hashes: hashes,
    };
    market.validate()?;
    let mut orders = Vec::new();
    for slot in 0..2000 {
        // 0 / 1 / 3 orders with probabilities .4 / .4 / .2: mean 1.
        let n = match rng.random_range(0..5) {
            0 | 1 => 0,
            2 | 3 => 1,
            _ => 3,
        };
        let hot_region = (slot / 200) % 8;
        for _ in 0..n {
            let pool = if rng.random_bool(0.65) {
                (hot_region as usize * 8 + rng.random_range(0..8)) % 128
            } else {
                rng.random_range(0..128)
            };
            let amount = match rng.random_range(0..10) {
                0..=3 => rng.random_range(1000..100_000),
                4..=8 => rng.random_range(100_000_000..3_000_000_000),
                _ => rng.random_range(3_000_000_000..15_000_000_000),
            };
            orders.push(UserOrder {
                slot,
                route: vec![Leg {
                    pool,
                    a_to_b: rng.random(),
                }],
                amount_in: amount,
                min_out: 1,
                wallet: rng.random_range(0..8),
            });
        }
    }
    let flow = PrivateFlow {
        version: 1,
        duration_slots: 2000,
        orders,
    };
    flow.validate(&market)?;
    world
        .surfnet
        .cheatcodes()
        .time_travel_to_slot_at(100_000, 1_800_000_000_000)?;
    write(out, "market.json", &market)?;
    write(out, "flow.json", &flow)?;
    write(out, "snapshot.json", &world.export()?)?;
    write(
        out,
        "provenance.json",
        &serde_json::json!({"generation_seed": hex::encode(seed), "programs": manifest, "base_slot":100000, "base_timestamp_ms":1800000000000u64, "feature_profile":"surfpool-pinned-default"}),
    )?;
    write(
        out,
        "users.json",
        &wallets
            .iter()
            .map(|k| k.to_bytes().to_vec())
            .collect::<Vec<_>>(),
    )?;
    write(out, "competition.json", &competition())?;
    write(
        out,
        "flow-manifest.json",
        &crate::injector::FlowManifest::create(out, None)?,
    )?;
    let seal = fixture::seal(
        out,
        &[
            "market.json",
            "flow.json",
            "snapshot.json",
            "provenance.json",
            "users.json",
            "competition.json",
            "flow-manifest.json",
        ],
    )?;
    write(out, "seal.json", &seal)?;
    Ok(seal.fixture_hash)
}

fn write(root: &Path, name: &str, value: &impl serde::Serialize) -> Result<()> {
    fs::write(root.join(name), serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

pub fn verify(root: &Path) -> Result<String> {
    let seal: fixture::Seal = serde_json::from_slice(&fs::read(root.join("seal.json"))?)?;
    let expected = [
        "market.json",
        "flow.json",
        "snapshot.json",
        "provenance.json",
        "users.json",
        "competition.json",
        "flow-manifest.json",
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        seal.files
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>()
            == expected,
        "incomplete fixture seal"
    );
    fixture::verify_seal(root, &seal)?;
    let market: Market = serde_json::from_slice(&fs::read(root.join("market.json"))?)?;
    market.validate()?;
    let flow: PrivateFlow = serde_json::from_slice(&fs::read(root.join("flow.json"))?)?;
    flow.validate(&market)?;
    verify_snapshot(root, &market)?;
    crate::injector::FlowManifest::verify(root)?;
    let frozen: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("competition.json"))?)?;
    ensure!(
        frozen == competition(),
        "reference competitors or execution policy changed"
    );
    Ok(seal.fixture_hash)
}

pub fn competition() -> serde_json::Value {
    use sha2::{Digest, Sha256};
    serde_json::json!({"version":4,"implementation_sha256":hex::encode(Sha256::digest([include_bytes!("runner.rs").as_slice(),include_bytes!("gateway.rs").as_slice(),include_bytes!("wallet.rs").as_slice(),include_bytes!("world.rs").as_slice(),include_bytes!("sdk.rs").as_slice(),include_bytes!("artifact.rs").as_slice(),include_bytes!("score.rs").as_slice(),include_bytes!("verifier.rs").as_slice()].concat())),"user_flow_engine":"swap-injector-v1","injector_sha256":hex::encode(Sha256::digest(include_bytes!("injector.rs"))),"protocol":crate::PROTOCOL,"competitors":crate::reference::strategies(),"runtime_features":"litesvm-0.18.1-mainnet-2026-10-15-no-overrides","launcher_sha256":hex::encode(Sha256::digest(include_bytes!("../../../bench/backrun/searcher.py"))),"reference_source_sha256":hex::encode(Sha256::digest(include_bytes!("reference.rs"))),"dependency_lock_sha256":hex::encode(Sha256::digest(include_bytes!("../../../Cargo.lock"))),"logical_slot_ms":crate::SLOT_MS,"ordering":"native-surfpool-arrival","atomic_guard":false,"rpc_requests_per_second":256,"simulations_per_second":16,"submissions_per_second":32,"inflight_per_searcher":8,"maximum_transaction_cu":crate::MAX_TRANSACTION_CU,"surfpool_revision":SURFPOOL_REV})
}

/// Re-version only execution policy; every existing market/flow/setup byte is
/// copied exactly. Neither randomness nor fixture generation is invoked.
pub fn migrate_execution(source: &Path, out: &Path) -> Result<String> {
    ensure!(!out.exists(), "migration output already exists");
    let old: fixture::Seal = serde_json::from_slice(&fs::read(source.join("seal.json"))?)?;
    fixture::verify_seal(source, &old)?;
    fs::create_dir_all(out)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(out, fs::Permissions::from_mode(0o700))?;
    }
    for name in [
        "market.json",
        "flow.json",
        "snapshot.json",
        "provenance.json",
        "users.json",
    ] {
        let expected = old.files.get(name).context("incomplete source fixture")?;
        fs::copy(source.join(name), out.join(name))?;
        ensure!(
            &fixture::hash_file(&out.join(name))? == expected,
            "fixture bytes changed"
        );
    }
    write(out, "competition.json", &competition())?;
    write(
        out,
        "flow-manifest.json",
        &crate::injector::FlowManifest::create(out, Some(old.fixture_hash))?,
    )?;
    let seal = fixture::seal(
        out,
        &[
            "market.json",
            "flow.json",
            "snapshot.json",
            "provenance.json",
            "users.json",
            "competition.json",
            "flow-manifest.json",
        ],
    )?;
    write(out, "seal.json", &seal)?;
    verify(out)
}

fn verify_snapshot(root: &Path, market: &Market) -> Result<()> {
    use sha2::{Digest, Sha256};
    let snapshot: BTreeMap<String, Option<surfpool_types::AccountSnapshot>> =
        serde_json::from_slice(&fs::read(root.join("snapshot.json"))?)?;
    ensure!(
        snapshot.values().all(Option::is_some),
        "unresolved offline account"
    );
    let account = |address: &str| -> Result<solana_account::Account> {
        snapshot
            .get(address)
            .and_then(Option::as_ref)
            .context("missing trusted account")?
            .to_account()
            .map_err(anyhow::Error::msg)
    };
    for (id, file) in [
        (crate::CPMM, "raydium_cp_swap.so"),
        (crate::WHIRLPOOL, "whirlpool.so"),
    ] {
        let program = account(id)?;
        ensure!(
            program.executable && program.data.len() == 36 && program.data[..4] == [2, 0, 0, 0],
            "invalid program loader account"
        );
        let address = solana_pubkey::Pubkey::new_from_array(program.data[4..36].try_into()?);
        let data = account(&address.to_string())?;
        ensure!(
            data.data.len() >= 45 && data.data[..4] == [3, 0, 0, 0] && data.data[12] == 0,
            "upgradeable AMM program"
        );
        let digest = hex::encode(Sha256::digest(&data.data[45..]));
        ensure!(
            market.program_hashes.get(file) == Some(&digest),
            "AMM snapshot binary mismatch"
        );
    }
    for mint in &market.tokens {
        let token = account(mint)?;
        ensure!(
            token.owner == key(TOKEN) && token.data.len() == 82,
            "invalid token mint"
        );
        ensure!(
            token.data[..4] == [0, 0, 0, 0] && token.data[46..50] == [0, 0, 0, 0],
            "mint or freeze authority remains"
        );
    }
    let retired = pda(key(crate::CPMM), &[b"backrun-retired-authority"]);
    for pool in &market.pools {
        let program = match pool.kind {
            fixture::PoolKind::ConstantProduct => key(crate::CPMM),
            fixture::PoolKind::Concentrated => key(crate::WHIRLPOOL),
        };
        ensure!(
            account(&pool.address)?.owner == program,
            "pool owner mismatch"
        );
        let config = account(&pool.config)?;
        ensure!(config.owner == program, "config owner mismatch");
        let offsets: &[usize] = if pool.kind == fixture::PoolKind::ConstantProduct {
            &[44, 76]
        } else {
            &[8, 40, 72]
        };
        for offset in offsets {
            ensure!(
                config.data.get(*offset..*offset + 32) == Some(retired.as_ref()),
                "setup configuration authority remains"
            );
        }
        for (i, vault) in pool.vaults.iter().enumerate() {
            let vault = account(vault)?;
            ensure!(
                vault.owner == key(TOKEN) && vault.data.len() == 165,
                "invalid pool vault"
            );
            ensure!(
                vault.data[..32] == *key(&market.tokens[pool.mints[i]]).as_ref(),
                "vault mint mismatch"
            );
        }
    }
    Ok(())
}

/// A distinct, filtered snapshot for experimentation. Excludes user wallets,
/// positions, LP ownership, setup wallet, provenance, and the future schedule.
pub fn public(fixture: &Path, out: &Path) -> Result<()> {
    use std::collections::BTreeSet;
    let hash = verify(fixture)?;
    ensure!(!out.exists(), "public export already exists");
    let market: Market = serde_json::from_slice(&fs::read(fixture.join("market.json"))?)?;
    let snapshot: BTreeMap<String, Option<surfpool_types::AccountSnapshot>> =
        serde_json::from_slice(&fs::read(fixture.join("snapshot.json"))?)?;
    let mut allowed = market.tokens.iter().cloned().collect::<BTreeSet<_>>();
    for pool in &market.pools {
        allowed.extend([
            pool.address.clone(),
            pool.config.clone(),
            pool.observation.clone(),
        ]);
        allowed.extend(pool.vaults.clone());
        allowed.extend(pool.tick_arrays.clone());
    }
    allowed.insert(pda(key(crate::CPMM), &[b"vault_and_lp_mint_auth_seed"]).to_string());
    for (address, account) in &snapshot {
        if let Some(account) = account
            && (account.executable
                || account.owner.starts_with("BPFLoader")
                || account.owner.starts_with("LoaderV4"))
        {
            allowed.insert(address.clone());
        }
    }
    let public = snapshot
        .into_iter()
        .filter(|(k, _)| allowed.contains(k))
        .collect::<BTreeMap<_, _>>();
    fs::create_dir_all(out)?;
    write(out, "market.json", &market)?;
    write(out, "playground.json", &public)?;
    write(
        out,
        "fixture.json",
        &serde_json::json!({"fixture_hash":hash}),
    )?;
    Ok(())
}

//! Trusted fixed-flow driver. Candidate native code runs exclusively in a
//! searcher container, never through Command::new or a shell in this process.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine, prelude::BASE64_STANDARD};
use serde_json::{Value, json};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use surfpool_sdk::cheatcodes::builders::DeployProgram;

use crate::{
    artifact,
    fixture::{Market, PrivateFlow},
    gateway::Gateway,
    injector::SwapInjector,
    programs::*,
    sdk::Context,
    wallet::Wallet,
    world::World,
};

pub const REFERENCE_EXECUTOR: Pubkey = Pubkey::new_from_array([69; 32]);
pub struct Options {
    pub fixture: PathBuf,
    pub programs: PathBuf,
    pub artifact: PathBuf,
    pub bootstrap: PathBuf,
    pub out: PathBuf,
    pub listen: String,
}

pub async fn run_session(options: Options) -> Result<Value> {
    ensure!(
        cfg!(target_os = "linux") && std::env::var("MTH_BACKRUN_ISOLATED").as_deref() == Ok("1"),
        "scoring requires the isolated Docker Compose verifier; use the playground for development"
    );
    ensure!(
        !options.out.join("reward.txt").exists(),
        "a scored run already exists; never overwrite or select the best repeat"
    );
    fs::create_dir_all(&options.out)?;
    let result = run(&options).await;
    match result {
        Ok(evidence) => {
            fs::write(
                options.out.join("session.json"),
                serde_json::to_vec_pretty(&evidence)?,
            )?;
            Ok(json!({"session_complete":true,"fixture_hash":evidence["fixture_hash"]}))
        }
        Err(error) => {
            fs::write(
                options.out.join("infrastructure-error.json"),
                serde_json::to_vec_pretty(
                    &json!({"status":"infrastructure_error","error":format!("{error:#}")}),
                )?,
            )?;
            Err(error)
        }
    }
}

async fn run(options: &Options) -> Result<Value> {
    let fixture_hash = crate::build::verify(&options.fixture)?;
    let mut market: Market =
        serde_json::from_slice(&fs::read(options.fixture.join("market.json"))?)?;
    market.hydrate();
    for (file, expected) in &market.program_hashes {
        ensure!(
            crate::fixture::hash_file(&options.programs.join(file))? == *expected,
            "trusted binary mismatch: {file}"
        );
    }
    let flow: PrivateFlow = serde_json::from_slice(&fs::read(options.fixture.join("flow.json"))?)?;
    let users: Vec<Vec<u8>> =
        serde_json::from_slice(&fs::read(options.fixture.join("users.json"))?)?;
    let users = users
        .iter()
        .map(|v| Keypair::try_from(v.as_slice()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut injector = SwapInjector::new(&flow, &market)?;
    let world = World::restore(&options.fixture.join("snapshot.json")).await?;
    world
        .surfnet
        .cheatcodes()
        .fund_sol(&world.payer().pubkey(), 1_000_000_000_000_000_000)?;
    world
        .surfnet
        .cheatcodes()
        .time_travel_to_slot_at(100_000, 1_800_000_000_000)?;
    let snapshot: BTreeMap<String, Option<surfpool_types::AccountSnapshot>> =
        serde_json::from_slice(&fs::read(options.fixture.join("snapshot.json"))?)?;
    let mut reserved = snapshot
        .keys()
        .map(|s| s.parse())
        .collect::<std::result::Result<BTreeSet<Pubkey>, _>>()?;
    reserved.extend([
        REFERENCE_EXECUTOR,
        artifact::program_data_address(&REFERENCE_EXECUTOR),
        Pubkey::default(),
        key(TOKEN),
        key(ATA_PROGRAM),
        solana_compute_budget_interface::id(),
    ]);
    let (manifest, artifact_hash) = match artifact::inspect(&options.artifact, &reserved) {
        Ok(artifact) => artifact,
        Err(error) => {
            return Ok(
                json!({"version":2,"protocol":crate::PROTOCOL,"fixture_hash":fixture_hash,"terminal_disqualification":format!("invalid artifact: {error}")}),
            );
        }
    };
    world.surfnet.cheatcodes().deploy(
        DeployProgram::new(REFERENCE_EXECUTOR)
            .so_path(options.programs.join("mth_backrun_executor.so")),
    )?;
    world.make_immutable(REFERENCE_EXECUTOR)?;
    for program in &manifest.programs {
        // Artifact shape/hash errors were rejected above. A trusted loader/RPC
        // failure here cannot be attributed to candidate misconduct.
        world
            .surfnet
            .cheatcodes()
            .deploy(
                DeployProgram::new(program.address.parse()?)
                    .so_path(options.artifact.join(&program.file)),
            )
            .context("custom program installation failed in trusted runtime")?;
        world.make_immutable(program.address.parse()?)?;
    }
    for scratch in &manifest.scratch {
        world.surfnet.cheatcodes().set_account(
            &scratch.address.parse()?,
            world
                .rpc
                .get_minimum_balance_for_rent_exemption(scratch.bytes)?,
            &vec![0; scratch.bytes],
            &scratch.owner.parse()?,
        )?;
    }
    let mut wallets = vec![];
    let mut contexts = vec![];
    let mut tokens = vec![];
    for i in 0..4 {
        let executor = if i == 0 && !manifest.executor.is_empty() {
            manifest.executor.parse()?
        } else {
            REFERENCE_EXECUTOR
        };
        let wallet = Wallet::prepare(&world, &market, executor).await?;
        let addresses = crate::wallet::lookup_addresses(&market, &wallet);
        let mut tables = vec![];
        for chunk in addresses.chunks(256) {
            tables.push(world.lookup_table(chunk.to_vec()).await?);
        }
        contexts.push(Context::new(i, market.clone(), &wallet, &tables));
        tokens.push(hex::encode(rand::random::<[u8; 32]>()));
        wallets.push(wallet);
    }
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::UpdateBlockProductionMode(
            surfpool_types::BlockProductionMode::Manual,
        ))?;
    world
        .surfnet
        .cheatcodes()
        .time_travel_to_slot_at(200_000, 1_800_000_000_000)?;
    world.advance_slot().await?;
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::CommandClock(
            None,
            surfpool_types::ClockCommand::Pause,
        ))?;
    // Persist exact pre-session state for maintainer replay diagnostics only.
    fs::write(
        options.out.join("session-snapshot.json"),
        serde_json::to_vec(&world.export()?)?,
    )?;
    fs::write(
        options.out.join("replay.json"),
        serde_json::to_vec(
            &json!({"base_slot":200000,"timestamp_ms":1800000000000u64,"fixture_hash":fixture_hash}),
        )?,
    )?;
    world.lock_mutations()?;
    let gateway = Gateway::new(
        contexts,
        tokens,
        world.surfnet.rpc_url().into(),
        world.surfnet.ws_url().into(),
    );
    let listener = tokio::net::TcpListener::bind(&options.listen).await?;
    let server_gateway = gateway.clone();
    let server = tokio::spawn(async move { axum::serve(listener, server_gateway.router()).await });
    // Separate, read-only searcher volumes receive only their own bootstrap.
    for i in 0..4 {
        let dir = options.bootstrap.join(i.to_string());
        fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))?;
        }
        if i == 0 {
            let artifact_dir = dir.join("artifact");
            fs::create_dir_all(&artifact_dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&artifact_dir, fs::Permissions::from_mode(0o755))?;
            }
            for name in ["run.sh", "manifest.json", manifest.executable.as_str()]
                .into_iter()
                .chain(manifest.programs.iter().map(|p| p.file.as_str()))
            {
                fs::copy(options.artifact.join(name), artifact_dir.join(name))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(
                        artifact_dir.join(name),
                        fs::Permissions::from_mode(0o555),
                    )?;
                }
            }
        }
        let temporary = dir.join("config.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec(
                &json!({"gateway":"http://main:8000", "token":gateway.tokens[i],"participant":i}),
            )?,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o444))?;
        }
        fs::rename(temporary, dir.join("config.json"))?;
    }
    let ready_deadline = Instant::now() + Duration::from_secs(90);
    while !gateway.state.lock().unwrap().ready.iter().all(|r| *r) {
        if Instant::now() >= ready_deadline {
            let ready = gateway.state.lock().unwrap().ready;
            ensure!(
                ready[1..].iter().all(|r| *r),
                "reference searcher startup timeout"
            );
            server.abort();
            return Ok(disqualified(
                &fixture_hash,
                &artifact_hash,
                "candidate did not complete startup".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // All searchers run continuously. Slots are produced by Surfpool's clock;
    // the injector submits normal swaps when their fixed slot becomes current.
    let base = world.rpc.get_slot()?;
    let initial = world.export()?;
    let initial_hash = world.rpc.get_latest_blockhash()?.to_string();
    fs::write(
        options.out.join("replay.json"),
        serde_json::to_vec(
            &json!({"base_slot":200000,"start_slot":base,"initial_blockhash":initial_hash,"timestamp_ms":1800000000000u64,"fixture_hash":fixture_hash}),
        )?,
    )?;
    let coalesced = world.surfnet.coalesced_slots();
    let started = Instant::now();
    let mut user_records = vec![];
    let mut timing = vec![];
    {
        let mut state = gateway.state.lock().unwrap();
        state.start_slot = base;
        state.end_slot = base + flow.duration_slots;
        state.running = true;
    }
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::UpdateBlockProductionMode(
            surfpool_types::BlockProductionMode::Clock,
        ))?;
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::CommandClock(
            None,
            surfpool_types::ClockCommand::Resume,
        ))?;
    for slot in 0..flow.duration_slots {
        ensure!(
            !gateway.state.lock().unwrap().exited[1..].iter().any(|v| *v),
            "reference searcher exited during session"
        );
        let slot_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            ensure!(
                Instant::now() < slot_deadline,
                "runtime clock stalled before logical slot {slot}"
            );
            let current = world.rpc.get_slot()?;
            ensure!(
                current <= base + slot,
                "injector missed logical slot {slot} (runtime slot {current})"
            );
            if current == base + slot {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let before = Instant::now();
        for execution in injector
            .inject_round(slot as usize, &users, &world, &market)
            .await?
        {
            user_records.push(json!({"kind":"user_order","slot":slot,"order":execution.order,"transaction":execution.transaction,"receipt":execution.receipt}));
        }
        timing.push(json!({"slot":slot,"elapsed_us":started.elapsed().as_micros(),"injection_us":before.elapsed().as_micros()}));
        ensure!(
            world.surfnet.coalesced_slots() == coalesced,
            "Surfpool clock fell behind; infrastructure timing failure"
        );
    }
    let end_deadline = Instant::now() + Duration::from_secs(2);
    while world.rpc.get_slot()? < base + flow.duration_slots {
        ensure!(
            Instant::now() < end_deadline,
            "runtime clock stalled at session end"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    gateway.state.lock().unwrap().running = false;
    gateway.quiesce().await?;
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::UpdateBlockProductionMode(
            surfpool_types::BlockProductionMode::Manual,
        ))?;
    // SlotForward is an ordered command barrier, and seals the final block.
    world.advance_slot().await?;
    world
        .surfnet
        .send_command(surfpool_types::SimnetCommand::CommandClock(
            None,
            surfpool_types::ClockCommand::Pause,
        ))?;
    let end = world.rpc.get_slot()?;
    gateway.state.lock().unwrap().finished = true;
    ensure!(
        world.surfnet.coalesced_slots() == coalesced,
        "Surfpool clock fell behind"
    );
    let elapsed = started.elapsed().as_millis();
    let final_snapshot = world.export()?;
    fs::write(
        options.out.join("final-snapshot.json"),
        serde_json::to_vec(&final_snapshot)?,
    )?;
    let submissions = gateway.state.lock().unwrap().trace.clone();
    let mut trace = fs::File::create(options.out.join("trace.jsonl"))?;
    let mut submission_owners = BTreeMap::new();
    for record in &submissions {
        if let (Some(signature), Some(participant), Some(arrival)) = (
            crate::gateway::submission_signature(record),
            record["participant"].as_u64(),
            record["arrival_us"].as_u64(),
        ) {
            let entry = submission_owners
                .entry(signature)
                .or_insert((arrival, participant as usize));
            if arrival < entry.0 {
                *entry = (arrival, participant as usize);
            }
        }
    }
    let user_signatures = user_records
        .iter()
        .map(|r| {
            let tx: solana_transaction::versioned::VersionedTransaction =
                wincode::deserialize(&BASE64_STANDARD.decode(r["transaction"].as_str().unwrap())?)?;
            Ok(tx.signatures[0].to_string())
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let mut included = BTreeSet::new();
    let mut mixed_routes = 0u64;
    let mut maximum_swaps = 0usize;
    let mut counts = [0u64; 4];
    let mut fees = [0u64; 4];
    let mut failures = [0u64; 4];
    let mut priorities = [0u64; 4];
    for actual_slot in base..=end {
        let block: Value=world.rpc.send(solana_rpc_client_api::request::RpcRequest::GetBlock,json!([actual_slot,{"encoding":"base64","commitment":"processed","maxSupportedTransactionVersion":1}]))?;
        for item in block["transactions"].as_array().into_iter().flatten() {
            let wire = item["transaction"][0]
                .as_str()
                .context("missing ledger transaction")?;
            let tx: solana_transaction::versioned::VersionedTransaction =
                wincode::deserialize(&BASE64_STANDARD.decode(wire)?)?;
            let signature = tx.signatures[0].to_string();
            ensure!(
                included.insert(signature.clone()),
                "duplicate ledger execution"
            );
            let payer = tx.message.static_account_keys()[0];
            let participant = if user_signatures.contains(&signature) {
                None
            } else {
                wallets
                    .iter()
                    .position(|w| w.trader.pubkey() == payer)
                    .or_else(|| submission_owners.get(&signature).map(|entry| entry.1))
            };
            let receipt: Value=world.rpc.send(solana_rpc_client_api::request::RpcRequest::GetTransaction,json!([signature,{"encoding":"json","commitment":"processed","maxSupportedTransactionVersion":1}]))?;
            if let Some(i) = participant {
                counts[i] += 1;
                fees[i] += receipt["meta"]["fee"].as_u64().context("missing fee")?;
                if !receipt["meta"]["err"].is_null() {
                    failures[i] += 1;
                }
                priorities[i] +=
                    crate::score::priority_fee(&tx, receipt["meta"]["fee"].as_u64().unwrap())?;
            }
            if participant.is_some() && receipt["meta"]["err"].is_null() {
                let mut keys = tx
                    .message
                    .static_account_keys()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                for kind in ["writable", "readonly"] {
                    keys.extend(
                        receipt["meta"]["loadedAddresses"][kind]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|v| v.as_str().map(str::to_owned)),
                    );
                }
                let mut calls = vec![];
                for ix in tx.message.instructions() {
                    if let Some(p) = keys.get(ix.program_id_index as usize) {
                        calls.push(p.as_str());
                    }
                }
                for group in receipt["meta"]["innerInstructions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    for ix in group["instructions"].as_array().into_iter().flatten() {
                        if let Some(p) = ix["programIdIndex"]
                            .as_u64()
                            .and_then(|n| keys.get(n as usize))
                        {
                            calls.push(p.as_str());
                        }
                    }
                }
                let swaps = calls
                    .iter()
                    .filter(|p| **p == crate::CPMM || **p == crate::WHIRLPOOL)
                    .count();
                maximum_swaps = maximum_swaps.max(swaps);
                if swaps >= 3 && calls.contains(&crate::CPMM) && calls.contains(&crate::WHIRLPOOL) {
                    mixed_routes += 1;
                }
            }
            writeln!(
                trace,
                "{}",
                json!({"kind":"execution","slot":actual_slot-base,"participant":participant,"signature":signature,"transaction":wire,"receipt":receipt})
            )?;
        }
    }
    for record in &user_records {
        let tx: solana_transaction::versioned::VersionedTransaction = wincode::deserialize(
            &BASE64_STANDARD.decode(record["transaction"].as_str().unwrap())?,
        )?;
        ensure!(
            included.contains(&tx.signatures[0].to_string()),
            "user order missing from ledger"
        );
    }
    fs::write(
        options.out.join("user-orders.json"),
        serde_json::to_vec(&user_records)?,
    )?;
    fs::write(
        options.out.join("submissions.json"),
        serde_json::to_vec(&submissions)?,
    )?;
    let mut reports = vec![];
    for (i, wallet) in wallets.iter().enumerate() {
        let scratch = if i == 0 {
            manifest
                .scratch
                .iter()
                .map(|s| s.address.clone())
                .collect::<Vec<_>>()
        } else {
            vec![]
        };
        let initial_value =
            crate::score::capital(&initial, &wallet.trader.pubkey().to_string(), &scratch)?;
        let final_value = crate::score::capital(
            &final_snapshot,
            &wallet.trader.pubkey().to_string(),
            &scratch,
        )?;
        let own = submissions
            .iter()
            .filter(|s| s["participant"] == i)
            .collect::<Vec<_>>();
        let rejected = own
            .iter()
            .filter(|s| !s["response"]["error"].is_null())
            .count();
        let missing = own
            .iter()
            .filter_map(|s| s["response"]["result"].as_str())
            .filter(|sig| !included.contains(*sig))
            .collect::<BTreeSet<_>>()
            .len();
        reports.push(json!({"participant":i,"wallet":wallet.trader.pubkey().to_string(),"scratch":scratch,"initial_value_lamports":initial_value,"final_value_lamports":final_value,"fees":fees[i],"priority_fees":priorities[i],"executions":counts[i],"included_failures":failures[i],"non_inclusions":missing,"rejections":rejected}));
    }
    server.abort();
    Ok(
        json!({"version":3,"protocol":crate::PROTOCOL,"completed_slots":flow.duration_slots,"fixture_hash":fixture_hash,"artifact_hash":artifact_hash,"participants":reports,"coverage":{"successful_mixed_multihop_routes":mixed_routes,"maximum_successful_swaps":maximum_swaps},"user_executions":user_records.len(),"user_failures":user_records.iter().filter(|r|!r["receipt"]["meta"]["err"].is_null()).count(),"timing":{"slots":timing,"elapsed_ms":elapsed,"coalesced_slots":0},"start_slot":base,"end_slot":end,"hardware":hardware()?,"runtime_binary_sha256":crate::fixture::hash_file(&std::env::current_exe()?)?}),
    )
}
fn disqualified(fixture: &str, artifact: &str, reason: String) -> Value {
    json!({"version":3,"protocol":crate::PROTOCOL,"fixture_hash":fixture,"artifact_hash":artifact,"terminal_disqualification":reason})
}
fn hardware() -> Result<Value> {
    use sha2::{Digest, Sha256};
    let cpu = fs::read_to_string("/proc/cpuinfo")?;
    let cpu = cpu
        .lines()
        .filter(|s| !s.starts_with("BogoMIPS") && !s.starts_with("cpu MHz"))
        .collect::<Vec<_>>()
        .join("\n");
    let data = json!({"architecture":std::env::consts::ARCH,"cpuinfo":cpu,"kernel":fs::read_to_string("/proc/sys/kernel/osrelease")?.trim(),"grader_cpu_set":fs::read_to_string("/sys/fs/cgroup/cpuset.cpus.effective")?.trim(),"grader_memory_max":fs::read_to_string("/sys/fs/cgroup/memory.max")?.trim(),"searcher_cpu_sets":["0","1","2","3"],"searcher_cpus":1,"searcher_memory_bytes":536870912,"profile":std::env::var("MTH_BACKRUN_HARDWARE_PROFILE")?});
    Ok(
        json!({"fingerprint":hex::encode(Sha256::digest(serde_json::to_vec(&data)?)),"details":data}),
    )
}

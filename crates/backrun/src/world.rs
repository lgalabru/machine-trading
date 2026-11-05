//! Actual offline AMM execution and trusted initial provisioning.
use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::Instruction;
use solana_keypair::{Keypair, read_keypair_file};
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_signer::Signer;
use solana_transaction::Transaction;
use surfpool_sdk::{Surfnet, cheatcodes::builders::DeployProgram};

use crate::{
    CPMM, WHIRLPOOL, WSOL,
    fixture::{Pool, PoolKind},
    programs::*,
};

pub struct World {
    pub surfnet: Surfnet,
    pub rpc: RpcClient,
}

impl World {
    pub async fn restore(snapshot: &Path) -> Result<Self> {
        let accounts: BTreeMap<String, Option<surfpool_types::AccountSnapshot>> =
            serde_json::from_slice(&std::fs::read(snapshot)?)?;
        ensure!(
            accounts.values().all(Option::is_some),
            "offline snapshots cannot contain unresolved accounts"
        );
        // At this pinned revision, startup emits into a 1024-entry Geyser
        // channel before starting its consumer. Restore program pairs at boot,
        // then ordinary accounts after Ready to avoid that upstream deadlock.
        let (programs, ordinary): (BTreeMap<_, _>, BTreeMap<_, _>) =
            accounts.into_iter().partition(|(_, a)| {
                let a = a.as_ref().unwrap();
                a.executable || a.owner.starts_with("BPFLoader") || a.owner.starts_with("LoaderV4")
            });
        ensure!(programs.len() < 512, "too many startup program accounts");
        let surfnet = Surfnet::builder()
            .offline(true)
            .slot_time_ms(crate::SLOT_MS)
            .snapshot(programs)
            .instruction_profiling(false)
            .start()
            .await?;
        drain_events(&surfnet);
        let rpc = RpcClient::new_with_commitment(
            surfnet.rpc_url().to_owned(),
            CommitmentConfig::processed(),
        );
        for (address, snapshot) in ordinary {
            let account = snapshot.unwrap().to_account().map_err(anyhow::Error::msg)?;
            surfnet.cheatcodes().execute(
                surfpool_sdk::cheatcodes::builders::SetAccount::new(address.parse()?)
                    .lamports(account.lamports)
                    .data(account.data)
                    .owner(account.owner)
                    .rent_epoch(account.rent_epoch)
                    .executable(account.executable),
            )?;
        }
        Ok(Self { surfnet, rpc })
    }

    /// Submit without preflight: an included failure is a real fee-paying
    /// outcome. Setup uses `send`, while evaluation must use this method.
    pub async fn execute(
        &self,
        tx: &solana_transaction::versioned::VersionedTransaction,
    ) -> Result<serde_json::Value> {
        let signature = self.rpc.send_transaction_with_config(
            tx,
            RpcSendTransactionConfig {
                skip_preflight: true,
                ..Default::default()
            },
        )?;
        for _ in 0..100 {
            let receipt: serde_json::Value = self.rpc.send(solana_rpc_client_api::request::RpcRequest::GetTransaction,
                serde_json::json!([signature.to_string(), {"encoding":"json", "commitment":"processed", "maxSupportedTransactionVersion":1}]))?;
            if !receipt.is_null() {
                return Ok(receipt);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let status = self.rpc.get_signature_statuses(&[signature])?;
        bail!(
            "runtime failed to return transaction receipt: {:?}",
            status.value
        )
    }
    pub async fn new(programs: &Path, cpmm_admin: &Path) -> Result<Self> {
        let payer = read_keypair_file(cpmm_admin).map_err(|e| anyhow::anyhow!("{e}"))?;
        let surfnet = Surfnet::builder()
            .offline(true)
            .slot_time_ms(crate::SLOT_MS)
            .payer(payer)
            .instruction_profiling(false)
            .start()
            .await?;
        surfnet
            .cheatcodes()
            .fund_sol(&surfnet.payer().pubkey(), 1_000_000_000_000_000_000)?;
        for (id, file) in [(CPMM, "raydium_cp_swap.so"), (WHIRLPOOL, "whirlpool.so")] {
            surfnet
                .cheatcodes()
                .deploy(DeployProgram::new(key(id)).so_path(programs.join(file)))?;
        }
        drain_events(&surfnet);
        let rpc = RpcClient::new_with_commitment(
            surfnet.rpc_url().to_owned(),
            CommitmentConfig::processed(),
        );
        let world = Self { surfnet, rpc };
        for id in [CPMM, WHIRLPOOL] {
            world.make_immutable(key(id))?;
        }
        Ok(world)
    }

    /// Deployment is trusted setup. Remove the upgrade authority before any
    /// candidate runs, then verify the actual loader account, not a manifest.
    pub fn make_immutable(&self, program: Pubkey) -> Result<()> {
        let account = self.rpc.get_account(&program)?;
        ensure!(
            account.executable
                && account.owner == key("BPFLoaderUpgradeab1e11111111111111111111111")
                && account.data.len() == 36,
            "unsupported program loader"
        );
        let address = Pubkey::new_from_array(account.data[4..36].try_into()?);
        let mut data = self.rpc.get_account(&address)?;
        ensure!(
            data.data.len() >= 45 && data.data[..4] == [3, 0, 0, 0],
            "invalid program data"
        );
        data.data[12..45].fill(0);
        self.surfnet
            .cheatcodes()
            .set_account(&address, data.lamports, &data.data, &data.owner)?;
        ensure!(
            self.rpc.get_account_data(&address)?[12] == 0,
            "program remains upgradeable"
        );
        Ok(())
    }

    pub fn payer(&self) -> &Keypair {
        self.surfnet.payer()
    }

    pub async fn advance_slot(&self) -> Result<()> {
        let before = self.rpc.get_slot()?;
        self.surfnet
            .send_command(surfpool_types::SimnetCommand::SlotForward(None))?;
        for _ in 0..100 {
            if self.rpc.get_slot()? > before {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        bail!("runtime failed to advance manual slot")
    }

    pub async fn send(
        &self,
        instructions: Vec<Instruction>,
        extra_signers: &[&Keypair],
    ) -> Result<()> {
        let mut ix = vec![ComputeBudgetInstruction::set_compute_unit_limit(1_400_000)];
        ix.extend(instructions);
        let hash = self.rpc.get_latest_blockhash()?;
        let mut signers = vec![self.payer()];
        signers.extend_from_slice(extra_signers);
        let legacy =
            Transaction::new_signed_with_payer(&ix, Some(&self.payer().pubkey()), &signers, hash);
        let tx: solana_transaction::versioned::VersionedTransaction =
            if wincode::serialize(&legacy)?.len() > 1232 {
                let keys: std::collections::BTreeSet<_> = ix
                    .iter()
                    .flat_map(|i| i.accounts.iter())
                    .filter(|a| !a.is_signer)
                    .map(|a| a.pubkey)
                    .collect();
                let table = self.lookup_table(keys.into_iter().collect()).await?;
                let hash = self.rpc.get_latest_blockhash()?;
                let message = solana_message::v0::Message::try_compile(
                    &self.payer().pubkey(),
                    &ix,
                    &[table],
                    hash,
                )?;
                solana_transaction::versioned::VersionedTransaction::try_new(
                    solana_message::VersionedMessage::V0(message),
                    &signers,
                )?
            } else {
                legacy.into()
            };
        let sim = self.rpc.simulate_transaction(&tx)?.value;
        if let Some(err) = sim.err {
            bail!(
                "transaction simulation failed: {err}\n{}",
                sim.logs.unwrap_or_default().join("\n")
            );
        }
        let sig = self.rpc.send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: true,
                ..Default::default()
            },
        )?;
        for _ in 0..200 {
            if let Some(status) = self
                .rpc
                .get_signature_statuses(&[sig])?
                .value
                .into_iter()
                .next()
                .flatten()
            {
                if let Some(err) = status.err {
                    bail!("transaction failed: {err}");
                }
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        bail!("transaction confirmation timeout")
    }

    pub async fn lookup_table(
        &self,
        addresses: Vec<Pubkey>,
    ) -> Result<solana_message::AddressLookupTableAccount> {
        use solana_address_lookup_table_interface::instruction::{
            create_lookup_table, extend_lookup_table, freeze_lookup_table,
        };
        ensure!(
            !addresses.is_empty() && addresses.len() <= 256,
            "lookup table size"
        );
        let payer = self.payer().pubkey();
        let (ix, table) = create_lookup_table(payer, payer, self.rpc.get_slot()?.saturating_sub(1));
        Box::pin(self.send(vec![ix], &[])).await?;
        for chunk in addresses.chunks(20) {
            Box::pin(self.send(
                vec![extend_lookup_table(
                    table,
                    payer,
                    Some(payer),
                    chunk.to_vec(),
                )],
                &[],
            ))
            .await?;
        }
        Box::pin(self.send(vec![freeze_lookup_table(table, payer)], &[])).await?;
        Ok(solana_message::AddressLookupTableAccount {
            key: table,
            addresses,
        })
    }

    pub async fn create_mint(&self) -> Result<Pubkey> {
        let mint = Keypair::new();
        self.send(
            vec![
                solana_system_interface::instruction::create_account(
                    &self.payer().pubkey(),
                    &mint.pubkey(),
                    self.rpc.get_minimum_balance_for_rent_exemption(82)?,
                    82,
                    &key(TOKEN),
                ),
                spl_token_interface::instruction::initialize_mint2(
                    &key(TOKEN),
                    &mint.pubkey(),
                    &self.payer().pubkey(),
                    None,
                    9,
                )?,
            ],
            &[&mint],
        )
        .await?;
        Ok(mint.pubkey())
    }

    pub async fn fund_tokens(&self, owner: Pubkey, mint: Pubkey, amount: u64) -> Result<()> {
        let mut ix = vec![spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(&self.payer().pubkey(), &owner, &mint, &key(TOKEN))];
        if mint == key(WSOL) {
            ix.push(solana_system_interface::instruction::transfer(
                &self.payer().pubkey(),
                &ata(owner, mint),
                amount,
            ));
            ix.push(spl_token_interface::instruction::sync_native(
                &key(TOKEN),
                &ata(owner, mint),
            )?);
        } else if amount > 0 {
            ix.push(spl_token_interface::instruction::mint_to(
                &key(TOKEN),
                &mint,
                &ata(owner, mint),
                &self.payer().pubkey(),
                &[],
                amount,
            )?);
        }
        self.send(ix, &[]).await
    }

    pub async fn revoke_mint(&self, mint: Pubkey) -> Result<()> {
        if mint != key(WSOL) {
            self.send(
                vec![spl_token_interface::instruction::set_authority(
                    &key(TOKEN),
                    &mint,
                    None,
                    spl_token_interface::instruction::AuthorityType::MintTokens,
                    &self.payer().pubkey(),
                    &[],
                )?],
                &[],
            )
            .await?;
        }
        Ok(())
    }

    pub async fn cpmm_config(&self, index: u16, fee: u64) -> Result<Pubkey> {
        let (config, instruction) = cpmm_config(self.payer().pubkey(), index, fee);
        // The program requires this fixed fee-receiver account even when the
        // configured creation fee is zero. Provision its native-token layout
        // during trusted setup; pool initialization itself is a real instruction.
        if self.rpc.get_account(&key(CPMM_FEE_RECEIVER)).is_err() {
            let mut data = vec![0u8; 165];
            data[..32].copy_from_slice(key(WSOL).as_ref());
            data[32..64].copy_from_slice(Pubkey::new_unique().as_ref());
            data[108] = 1;
            data[109..113].copy_from_slice(&1u32.to_le_bytes());
            let rent = self.rpc.get_minimum_balance_for_rent_exemption(165)?;
            data[113..121].copy_from_slice(&rent.to_le_bytes());
            self.surfnet.cheatcodes().set_account(
                &key(CPMM_FEE_RECEIVER),
                rent,
                &data,
                &key(TOKEN),
            )?;
        }
        self.send(vec![instruction], &[]).await?;
        Ok(config)
    }

    pub async fn create_cpmm(
        &self,
        config: Pubkey,
        tokens: &[Pubkey],
        mut edge: [usize; 2],
        amount: u64,
        fee: u32,
    ) -> Result<Pool> {
        if tokens[edge[0]] > tokens[edge[1]] {
            edge.swap(0, 1);
        }
        let init = cpmm_initialize(
            self.payer().pubkey(),
            config,
            edge.map(|i| tokens[i]),
            [amount; 2],
        );
        self.send(vec![init.instruction], &[])
            .await
            .context("initialize CPMM")?;
        Ok(Pool {
            kind: PoolKind::ConstantProduct,
            address: init.state.to_string(),
            config: config.to_string(),
            mints: edge,
            vaults: init.vaults.map(|k| k.to_string()),
            observation: init.observation.to_string(),
            fee_millionths: fee,
            tick_spacing: 0,
            tick_arrays: vec![],
            token_addresses: tokens.iter().map(ToString::to_string).collect(),
        })
    }

    pub async fn whirlpool_config(
        &self,
        admin: &Keypair,
        spacing: u16,
        fee: u16,
    ) -> Result<Pubkey> {
        let config = Keypair::new();
        self.surfnet
            .cheatcodes()
            .fund_sol(&admin.pubkey(), 100_000_000_000)?;
        let authority = self.payer().pubkey();
        let mut args = Vec::new();
        for _ in 0..3 {
            args.extend_from_slice(authority.as_ref());
        }
        args.extend_from_slice(&0u16.to_le_bytes());
        self.send(
            vec![anchor(
                key(WHIRLPOOL),
                "initialize_config",
                vec![
                    signer(config.pubkey()),
                    signer(admin.pubkey()),
                    ro(Pubkey::default()),
                ],
                &args,
            )],
            &[admin, &config],
        )
        .await?;
        let tier = pda(
            key(WHIRLPOOL),
            &[
                b"fee_tier",
                config.pubkey().as_ref(),
                &spacing.to_le_bytes(),
            ],
        );
        let mut args = spacing.to_le_bytes().to_vec();
        args.extend_from_slice(&fee.to_le_bytes());
        self.send(
            vec![anchor(
                key(WHIRLPOOL),
                "initialize_fee_tier",
                vec![
                    ro(config.pubkey()),
                    rw(tier),
                    signer(authority),
                    ro_signer(authority),
                    ro(Pubkey::default()),
                ],
                &args,
            )],
            &[],
        )
        .await?;
        Ok(config.pubkey())
    }

    pub async fn create_whirlpool(
        &self,
        config: Pubkey,
        tokens: &[Pubkey],
        mut edge: [usize; 2],
        spacing: u16,
        fee: u32,
        liquidity: u128,
    ) -> Result<Pool> {
        if tokens[edge[0]] > tokens[edge[1]] {
            edge.swap(0, 1);
        }
        let mints = edge.map(|i| tokens[i]);
        let (pool, bump) = whirlpool_address(config, mints, spacing);
        let vault_a = Keypair::new();
        let vault_b = Keypair::new();
        let owner = self.payer().pubkey();
        let tier = pda(
            key(WHIRLPOOL),
            &[b"fee_tier", config.as_ref(), &spacing.to_le_bytes()],
        );
        let mut args = vec![bump];
        args.extend_from_slice(&spacing.to_le_bytes());
        args.extend_from_slice(&(1u128 << 64).to_le_bytes());
        self.send(
            vec![anchor(
                key(WHIRLPOOL),
                "initialize_pool",
                vec![
                    ro(config),
                    ro(mints[0]),
                    ro(mints[1]),
                    signer(owner),
                    rw(pool),
                    signer(vault_a.pubkey()),
                    signer(vault_b.pubkey()),
                    ro(tier),
                    ro(key(TOKEN)),
                    ro(Pubkey::default()),
                    ro(key(RENT)),
                ],
                &args,
            )],
            &[&vault_a, &vault_b],
        )
        .await?;
        let span = i32::from(spacing) * 88;
        let mut arrays = Vec::new();
        for n in -4..=3 {
            let start = n * span;
            let array = tick_array(pool, start);
            self.send(
                vec![anchor(
                    key(WHIRLPOOL),
                    "initialize_tick_array",
                    vec![ro(pool), signer(owner), rw(array), ro(Pubkey::default())],
                    &start.to_le_bytes(),
                )],
                &[],
            )
            .await?;
            arrays.push(array.to_string());
        }
        // Three overlapping bands create meaningful liquidity changes as ticks
        // cross. Every position is initialized and funded through the program.
        for (lower, upper, weight) in [
            (-3 * span, 3 * span, 1u128),
            (-span, span, 3),
            (-4 * i32::from(spacing), 4 * i32::from(spacing), 2),
        ] {
            let mint = Keypair::new();
            let (position, bump) = Pubkey::find_program_address(
                &[b"position", mint.pubkey().as_ref()],
                &key(WHIRLPOOL),
            );
            let mut args = vec![bump];
            args.extend_from_slice(&lower.to_le_bytes());
            args.extend_from_slice(&upper.to_le_bytes());
            self.send(
                vec![anchor(
                    key(WHIRLPOOL),
                    "open_position",
                    vec![
                        signer(owner),
                        ro(owner),
                        rw(position),
                        signer(mint.pubkey()),
                        rw(ata(owner, mint.pubkey())),
                        ro(pool),
                        ro(key(TOKEN)),
                        ro(Pubkey::default()),
                        ro(key(RENT)),
                        ro(key(ATA_PROGRAM)),
                    ],
                    &args,
                )],
                &[&mint],
            )
            .await?;
            let mut args = (liquidity * weight).to_le_bytes().to_vec();
            args.extend_from_slice(&u64::MAX.to_le_bytes());
            args.extend_from_slice(&u64::MAX.to_le_bytes());
            self.send(
                vec![anchor(
                    key(WHIRLPOOL),
                    "increase_liquidity",
                    vec![
                        rw(pool),
                        ro(key(TOKEN)),
                        ro_signer(owner),
                        rw(position),
                        ro(ata(owner, mint.pubkey())),
                        rw(ata(owner, mints[0])),
                        rw(ata(owner, mints[1])),
                        rw(vault_a.pubkey()),
                        rw(vault_b.pubkey()),
                        rw(tick_array(pool, lower.div_euclid(span) * span)),
                        rw(tick_array(pool, upper.div_euclid(span) * span)),
                    ],
                    &args,
                )],
                &[],
            )
            .await?;
        }
        Ok(Pool {
            kind: PoolKind::Concentrated,
            address: pool.to_string(),
            config: config.to_string(),
            mints: edge,
            vaults: [vault_a.pubkey().to_string(), vault_b.pubkey().to_string()],
            observation: pda(key(WHIRLPOOL), &[b"oracle", pool.as_ref()]).to_string(),
            fee_millionths: fee,
            tick_spacing: spacing,
            tick_arrays: arrays,
            token_addresses: tokens.iter().map(ToString::to_string).collect(),
        })
    }

    pub fn token_balance(&self, owner: Pubkey, mint: Pubkey) -> Result<u64> {
        Ok(self
            .rpc
            .get_token_account_balance(&ata(owner, mint))?
            .amount
            .parse()?)
    }

    pub async fn swap(
        &self,
        pool: &Pool,
        direction: bool,
        amount: u64,
        min_out: u64,
    ) -> Result<()> {
        let ix = match pool.kind {
            PoolKind::ConstantProduct => {
                cpmm_swap(pool, self.payer().pubkey(), direction, amount, min_out)
            }
            PoolKind::Concentrated => {
                let data = self.rpc.get_account_data(&key(&pool.address))?;
                ensure!(data.len() >= 85, "invalid whirlpool state");
                let tick = i32::from_le_bytes(data[81..85].try_into()?);
                whirlpool_swap(
                    pool,
                    self.payer().pubkey(),
                    direction,
                    amount,
                    min_out,
                    tick,
                )?
            }
        };
        self.send(vec![ix], &[]).await
    }

    /// Lock every administrative RPC except the read-only snapshot export.
    /// The gateway never exposes any surfnet_* method, including this export.
    pub fn lock_mutations(&self) -> Result<()> {
        self.surfnet
            .cheatcodes()
            .lock(surfpool_sdk::CheatcodeFilter::list(
                surfpool_types::SURFNET_CHEATCODE_METHODS
                    .into_iter()
                    .filter(|m| *m != "surfnet_exportSnapshot"),
            ))?;
        Ok(())
    }

    pub fn export(&self) -> Result<serde_json::Value> {
        let v: serde_json::Value = self.rpc.send(solana_rpc_client_api::request::RpcRequest::Custom { method: "surfnet_exportSnapshot" }, serde_json::json!([{ "scope": "network", "filter": { "includeProgramAccounts": true, "excludeSysvars": true } }]))?;
        let accounts = v.get("value").unwrap_or(&v).clone();
        // Ensure export is an account mapping, not an RPC envelope.
        let _: BTreeMap<String, Option<surfpool_types::AccountSnapshot>> =
            serde_json::from_value(accounts.clone())?;
        Ok(accounts)
    }
}

fn ro_signer(key: Pubkey) -> solana_instruction::AccountMeta {
    solana_instruction::AccountMeta::new_readonly(key, true)
}

/// SDK events include lossless control notifications. Drain from startup so a
/// full diagnostic buffer cannot block clock pause/resume after large setup.
fn drain_events(surfnet: &Surfnet) {
    let events = surfnet.events().clone();
    std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match event {
                surfpool_types::SimnetEvent::ErrorLog(_, message)
                | surfpool_types::SimnetEvent::Aborted(message) => eprintln!("Surfpool: {message}"),
                surfpool_types::SimnetEvent::Shutdown => break,
                _ => {}
            }
        }
    });
}

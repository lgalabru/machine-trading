//! Trusted swap injector. Fixed orders are signed and executed through the
//! normal Surfpool transaction path at fixed slots during continuous trading.
use std::{fs, path::Path};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::fixture::{PrivateFlow, hash_file};

/// Pins existing assets without duplicating the order schedule in another file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FlowManifest {
    pub version: u32,
    pub engine: String,
    pub protocol: String,
    pub market_sha256: String,
    pub flow_sha256: String,
    pub previous_fixture_hash: Option<String>,
}
impl FlowManifest {
    pub fn create(root: &Path, previous_fixture_hash: Option<String>) -> Result<Self> {
        Ok(Self {
            version: 1,
            engine: "swap-injector-v1".into(),
            protocol: crate::PROTOCOL.into(),
            market_sha256: hash_file(&root.join("market.json"))?,
            flow_sha256: hash_file(&root.join("flow.json"))?,
            previous_fixture_hash,
        })
    }
    pub fn verify(root: &Path) -> Result<()> {
        let actual: Self = serde_json::from_slice(&fs::read(root.join("flow-manifest.json"))?)?;
        ensure!(
            actual == Self::create(root, actual.previous_fixture_hash.clone())?,
            "fixed market, flow or injector manifest changed"
        );
        FlowSchedule::load(root)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Round {
    pub round: u64,
    pub orders: Vec<usize>,
}
/// Derived exclusively from flow.json, including its quiet rounds and bursts.
pub struct FlowSchedule {
    pub rounds: Vec<Round>,
}
impl FlowSchedule {
    pub fn load(root: &Path) -> Result<Self> {
        Self::from_flow(&serde_json::from_slice(&fs::read(root.join("flow.json"))?)?)
    }
    pub fn from_flow(flow: &PrivateFlow) -> Result<Self> {
        ensure!(
            flow.version == 1 && flow.duration_slots == 2000,
            "unsupported user-flow duration"
        );
        let mut rounds = (0..flow.duration_slots)
            .map(|round| Round {
                round,
                orders: vec![],
            })
            .collect::<Vec<_>>();
        let mut previous = 0;
        for (index, order) in flow.orders.iter().enumerate() {
            ensure!(
                order.slot >= previous && order.slot < flow.duration_slots,
                "unordered or out-of-range user order"
            );
            rounds[order.slot as usize].orders.push(index);
            previous = order.slot;
        }
        Ok(Self { rounds })
    }
}

pub struct SwapInjector<'a> {
    flow: &'a PrivateFlow,
    schedule: FlowSchedule,
    next_round: usize,
    failed: bool,
}
pub struct UserExecution {
    pub order: usize,
    pub pool: usize,
    pub transaction: String,
    pub receipt: serde_json::Value,
}
impl<'a> SwapInjector<'a> {
    pub fn new(flow: &'a PrivateFlow, market: &crate::fixture::Market) -> Result<Self> {
        flow.validate(market)?;
        Ok(Self {
            flow,
            schedule: FlowSchedule::from_flow(flow)?,
            next_round: 0,
            failed: false,
        })
    }
    pub async fn inject_round(
        &mut self,
        round: usize,
        users: &[solana_keypair::Keypair],
        world: &crate::world::World,
        market: &crate::fixture::Market,
    ) -> Result<Vec<UserExecution>> {
        use base64::{Engine, prelude::BASE64_STANDARD};
        use solana_signer::Signer;

        use crate::{fixture::PoolKind, programs::*};
        ensure!(
            !self.failed,
            "injector stopped after an infrastructure error"
        );
        ensure!(
            round == self.next_round,
            "user-flow rounds must execute once, in order"
        );
        let step = self
            .schedule
            .rounds
            .get(round)
            .ok_or_else(|| anyhow::anyhow!("user flow completed"))?;
        // Fail closed if execution cannot produce a receipt. An included swap
        // failure is a normal receipt and does not poison the injector.
        self.failed = true;
        let mut executions = vec![];
        for &index in &step.orders {
            let order = &self.flow.orders[index];
            ensure!(
                order.slot == step.round && order.route.len() == 1,
                "fixed user order mismatch"
            );
            let leg = &order.route[0];
            let pool = &market.pools[leg.pool];
            let user = users
                .get(order.wallet)
                .ok_or_else(|| anyhow::anyhow!("missing user signing key"))?;
            let swap = match pool.kind {
                PoolKind::ConstantProduct => cpmm_swap(
                    pool,
                    user.pubkey(),
                    leg.a_to_b,
                    order.amount_in,
                    order.min_out,
                ),
                PoolKind::Concentrated => {
                    let bytes = world.rpc.get_account_data(&key(&pool.address))?;
                    whirlpool_swap(
                        pool,
                        user.pubkey(),
                        leg.a_to_b,
                        order.amount_in,
                        order.min_out,
                        i32::from_le_bytes(bytes[81..85].try_into()?),
                    )?
                }
            };
            let tx=solana_transaction::Transaction::new_signed_with_payer(&[solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_limit(crate::USER_TRANSACTION_CU),swap],Some(&user.pubkey()),&[user],world.rpc.get_latest_blockhash()?);
            let receipt = world.execute(&tx.clone().into()).await?;
            executions.push(UserExecution {
                order: index,
                pool: leg.pool,
                transaction: BASE64_STANDARD.encode(wincode::serialize(&tx)?),
                receipt,
            });
        }
        self.next_round += 1;
        self.failed = false;
        Ok(executions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_flow_preserves_quiet_rounds_bursts_and_rejects_invalid_slots() {
        let flow: PrivateFlow = serde_json::from_str(r#"{"version":1,"duration_slots":2000,"orders":[{"slot":4,"wallet":0,"route":[{"pool":0,"a_to_b":true}],"amount_in":100,"min_out":99},{"slot":4,"wallet":0,"route":[{"pool":1,"a_to_b":true}],"amount_in":200,"min_out":199}]}"#).unwrap();
        let schedule = FlowSchedule::from_flow(&flow).unwrap();
        assert_eq!(schedule.rounds.len(), 2000);
        assert!(schedule.rounds[0].orders.is_empty());
        assert_eq!(schedule.rounds[4].orders, vec![0, 1]);
        let mut bad = flow.clone();
        bad.orders[1].slot = 3;
        assert!(FlowSchedule::from_flow(&bad).is_err());
        bad.orders[1].slot = 2000;
        assert!(FlowSchedule::from_flow(&bad).is_err());
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("market.json"), b"fixed market bytes").unwrap();
        fs::write(
            root.path().join("flow.json"),
            serde_json::to_vec(&flow).unwrap(),
        )
        .unwrap();
        let manifest = FlowManifest::create(root.path(), Some("parent".into())).unwrap();
        fs::write(
            root.path().join("flow-manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        FlowManifest::verify(root.path()).unwrap();
        fs::write(
            root.path().join("flow.json"),
            serde_json::to_vec(&bad).unwrap(),
        )
        .unwrap();
        assert!(FlowManifest::verify(root.path()).is_err());
    }
}

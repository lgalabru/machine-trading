//! A competing liquidator on Phoenix perps: the keeper a scenario can opt
//! into (`liquidator`, see [`Liquidator`]).
//!
//! Without it, a bot that crosses maintenance margin is not liquidated
//! during the session, and one that ends liquidatable is disqualified. With
//! it, a grader-held account liquidates the bot mid-session through Phoenix's
//! own instructions, the way mainnet keepers do, and the bot keeps whatever
//! the liquidation leaves it. The cost (Phoenix's fill through the book
//! and its fees) is in the account's value, so the score prices it.
//!
//! The keeper is a [`VenueCrank`]: after each step's blocks land it reads
//! Phoenix's margin view of the bot's trader accounts (every one of them,
//! the cross-margin account and any subaccount, as the meter does). When one
//! reads liquidatable, the keeper waits out its reaction time, drawn once per
//! session from the session's seed, then sends `LiquidateViaMarketOrder`,
//! or `LiquidationTransfer` once the account reads past the backstop margin,
//! and does so again each turn while the account stays liquidatable. Phoenix
//! decides what each call closes. The keeper never computes a liquidation.
//!
//! Each stretch of liquidations ends when the account reads healthy again,
//! and is one [`ForcedClose`] in the report (reason
//! [`ForcedCloseReason::Liquidation`]), holding the keeper's orders. The
//! instruction layouts come from Phoenix's IDL (bundled with surfpool): the
//! pinned `phoenix-rise` has no builder for them, and a test checks the
//! encoding against a mainnet transaction.

use std::{str::FromStr, sync::Arc};

use async_trait::async_trait;
use mtx_core::{AccountMeter, InstrumentId};
use mtx_kit::{
    chain::{ChainClient, RpcChainClient},
    phoenix_rise::{
        accounts::{PhoenixAccount, trader::TraderHeader},
        core::PhoenixHawkeyeClient,
        ix::{PHOENIX_INSTRUCTION_ADDRESSES, PHOENIX_PROGRAM_ID, PhoenixInstruction},
        types::exchange::ExchangeMarketConfig,
    },
    protocols::phoenix::eternal::{PhoenixEternalMeter, PhoenixExchange},
};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_signer::Signer;
use solana_transaction::{AccountMeta, Instruction};
use tracing::{info, warn};

use crate::{
    Error, Result,
    crank::{CrankOutcome, CrankStep, VenueCrank},
    risk::{CloseOrder, ForcedClose, ForcedCloseReason, LiquidatedBy, is_flat},
    scenario::SplitMix64,
    session::{INTERNAL_RPC_TIMEOUT, grader_f64, patiently},
};

/// How far through the mark a liquidation's market order may fill, as a
/// share of the mark: the 10% mainnet keepers use. A liquidation's own
/// penalty is Phoenix's, inside the order's fill; this only bounds the walk
/// through a thin book.
const LIQUIDATION_BAND: f64 = 0.10;

/// The keeper's reaction time and cadence, in a scenario's
/// `venues.perp.liquidator` (v2) or `liquidator` (v1).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Liquidator {
    /// The slots between the first step the account reads liquidatable and
    /// the keeper's first liquidation, `[min, max]`: drawn once per session
    /// from its seed. It models a competing keeper's latency, and the bot
    /// is told the range, never the draw.
    pub reaction_slots: [u64; 2],
    /// How often the keeper looks, in steps. Every step by default.
    #[serde(default = "every_step")]
    pub check_every_steps: u64,
}

fn every_step() -> u64 {
    1
}

impl Liquidator {
    /// A range that holds, and a cadence of at least one step.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let [min, max] = self.reaction_slots;
        if min > max {
            return Err(format!(
                "liquidator: reaction_slots [{min}, {max}] has its minimum above its maximum"
            ));
        }
        if self.check_every_steps == 0 {
            return Err("liquidator: check_every_steps must be at least 1".into());
        }
        Ok(())
    }

    /// The session's reaction time: one draw from `seed`.
    pub fn delay_slots(&self, seed: [u8; 32]) -> u64 {
        let [min, max] = self.reaction_slots;
        let word = seed
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
            .fold(0u64, |acc, word| acc.rotate_left(17) ^ word);
        SplitMix64(word).range_u64((min, max))
    }
}

/// What the keeper did in a session, for its report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LiquidatorReport {
    /// The reaction time this session drew, in slots. Never in the bot's view.
    pub delay_slots: u64,
    /// The liquidations that landed.
    pub liquidations: u32,
    /// What they cost the account: the sum, over the stretches the keeper
    /// liquidated through, of the value before the first liquidation less
    /// the value after the last. Phoenix's penalty and the fill's impact.
    pub cost_usd: f64,
}

/// Phoenix's accounts that every liquidation names, read from the
/// exchange's metadata, and the market's.
#[derive(Debug, Clone)]
pub struct Keys {
    global_config: Pubkey,
    perp_asset_map: Pubkey,
    global_trader_index: Vec<Pubkey>,
    active_trader_buffer: Vec<Pubkey>,
    mint: Pubkey,
    orderbook: Pubkey,
    splines: Pubkey,
}

impl Keys {
    pub fn of(exchange: &PhoenixExchange, market: &ExchangeMarketConfig) -> Result<Self> {
        let keys = exchange.metadata().keys();
        let parse = |key: &str| {
            Pubkey::from_str(key).map_err(|_| Error::InvalidExchangeKey(key.to_string()))
        };
        Ok(Self {
            global_config: PHOENIX_INSTRUCTION_ADDRESSES.global_configuration,
            perp_asset_map: parse(&keys.perp_asset_map)?,
            global_trader_index: keys
                .global_trader_index
                .iter()
                .map(|k| parse(k))
                .collect::<Result<_>>()?,
            active_trader_buffer: keys
                .active_trader_buffer
                .iter()
                .map(|k| parse(k))
                .collect::<Result<_>>()?,
            mint: parse(&keys.canonical_mint)?,
            orderbook: parse(&market.market_pubkey)?,
            splines: parse(&market.spline_pubkey)?,
        })
    }

    /// The accounts after the signer's, in the order Phoenix's IDL lists
    /// them for the instructions that touch a market's book.
    fn market_accounts(&self) -> Vec<AccountMeta> {
        let mut accounts = vec![AccountMeta::new(self.perp_asset_map, false)];
        accounts.extend(
            self.global_trader_index
                .iter()
                .map(|k| AccountMeta::new(*k, false)),
        );
        accounts.extend(
            self.active_trader_buffer
                .iter()
                .map(|k| AccountMeta::new(*k, false)),
        );
        accounts
    }

    fn book_accounts(&self) -> [AccountMeta; 2] {
        [
            AccountMeta::new(self.orderbook, false),
            AccountMeta::new(self.splines, false),
        ]
    }

    fn head(&self) -> [AccountMeta; 3] {
        [
            AccountMeta::new_readonly(*PHOENIX_PROGRAM_ID, false),
            AccountMeta::new_readonly(PHOENIX_INSTRUCTION_ADDRESSES.log_authority, false),
            AccountMeta::new(self.global_config, false),
        ]
    }
}

/// `LiquidateViaMarketOrder`: `keeper` closes up to `size` base lots of
/// `trader`'s position with a market order on the book, filling no further
/// than `limit_ticks`. Phoenix refuses it unless the trader is liquidatable
/// and has no risk-increasing orders resting.
pub fn liquidate_via_market_order(
    keys: &Keys,
    keeper: Pubkey,
    trader: Pubkey,
    size: u64,
    limit_ticks: u64,
) -> Instruction {
    let mut data = PhoenixInstruction::LiquidateViaMarketOrder
        .discriminant()
        .to_vec();
    data.extend(keys.mint.to_bytes());
    data.extend(size.to_le_bytes());
    data.extend(limit_ticks.to_le_bytes());
    data.push(0); // fill_or_kill
    let mut accounts = keys.head().to_vec();
    accounts.extend([
        AccountMeta::new_readonly(keeper, true),
        AccountMeta::new(trader, false),
        AccountMeta::new(trader, false),
    ]);
    accounts.extend(keys.market_accounts());
    accounts.extend(keys.book_accounts());
    Instruction {
        program_id: *PHOENIX_PROGRAM_ID,
        accounts,
        data,
    }
}

/// `ForceCancelRiskIncreasing`: cancels `trader`'s risk-increasing orders
/// on the market, which Phoenix lets any signer do while the trader is
/// liquidatable. A liquidation needs them gone first.
pub fn force_cancel_risk_increasing(keys: &Keys, keeper: Pubkey, trader: Pubkey) -> Instruction {
    let mut accounts = keys.head().to_vec();
    accounts.extend([
        AccountMeta::new_readonly(keeper, true),
        AccountMeta::new(trader, false),
    ]);
    accounts.extend(keys.market_accounts());
    accounts.extend(keys.book_accounts());
    Instruction {
        program_id: *PHOENIX_PROGRAM_ID,
        accounts,
        data: PhoenixInstruction::ForceCancelRiskIncreasing
            .discriminant()
            .to_vec(),
    }
}

/// `LiquidationTransfer`, the backstop: moves `base_lots` of the market's
/// position from `liquidatee` to the keeper's own trader account, which
/// Phoenix allows once the liquidatee reads past the backstop margin.
pub fn liquidation_transfer(
    keys: &Keys,
    keeper: Pubkey,
    keeper_trader: Pubkey,
    liquidatee: Pubkey,
    asset_id: u64,
    base_lots: u64,
) -> Instruction {
    let mut data = PhoenixInstruction::LiquidationTransfer
        .discriminant()
        .to_vec();
    data.extend(1u32.to_le_bytes());
    data.extend(asset_id.to_le_bytes());
    data.extend(base_lots.to_le_bytes());
    data.push(1); // execute_all_transfers
    let mut accounts = keys.head().to_vec();
    accounts.extend([
        AccountMeta::new(keys.perp_asset_map, false),
        AccountMeta::new_readonly(keeper, true),
        AccountMeta::new(keeper_trader, false),
        AccountMeta::new(liquidatee, false),
    ]);
    accounts.extend(
        keys.global_trader_index
            .iter()
            .chain(&keys.active_trader_buffer)
            .map(|k| AccountMeta::new(*k, false)),
    );
    Instruction {
        program_id: *PHOENIX_PROGRAM_ID,
        accounts,
        data,
    }
}

/// A stretch of liquidations in progress.
struct Episode {
    step: u64,
    at: u64,
    value_before_usd: f64,
    position_before: f64,
    value_after_usd: f64,
    orders: Vec<CloseOrder>,
    position_after: f64,
}

/// The keeper: see the module docs.
pub(crate) struct LiquidationKeeper {
    spec: Liquidator,
    delay_slots: u64,
    wallet: Keypair,
    wallet_trader: Pubkey,
    exchange: Arc<PhoenixExchange>,
    rpc_url: String,
    chain: RpcChainClient,
    keys: Keys,
    market: ExchangeMarketConfig,
    owner: Pubkey,
    main_trader: Pubkey,
    meter: PhoenixEternalMeter,
    universe: Vec<InstrumentId>,
    market_id: InstrumentId,
    /// The slot (counted from the session's start) the account first read
    /// liquidatable in the current breach, if it does now.
    first_seen: Option<u64>,
    episode: Option<Episode>,
    closes: Vec<ForcedClose>,
    liquidations: u32,
    cost_usd: f64,
}

/// What the keeper needs to know of the session.
pub(crate) struct KeeperSetup {
    pub spec: Liquidator,
    pub seed: [u8; 32],
    pub wallet: Keypair,
    pub exchange: Arc<PhoenixExchange>,
    pub rpc_url: String,
    pub market: ExchangeMarketConfig,
    pub owner: Pubkey,
    pub meter: PhoenixEternalMeter,
    pub universe: Vec<InstrumentId>,
    pub market_id: InstrumentId,
}

impl LiquidationKeeper {
    pub(crate) fn new(setup: KeeperSetup) -> Result<Self> {
        let keys = Keys::of(&setup.exchange, &setup.market)?;
        Ok(Self {
            delay_slots: setup.spec.delay_slots(setup.seed),
            wallet_trader: setup.exchange.trader_account(&setup.wallet.pubkey()),
            main_trader: setup.exchange.trader_account(&setup.owner),
            chain: RpcChainClient::with_timeout(&setup.rpc_url, INTERNAL_RPC_TIMEOUT),
            spec: setup.spec,
            wallet: setup.wallet,
            exchange: setup.exchange,
            rpc_url: setup.rpc_url,
            keys,
            market: setup.market,
            owner: setup.owner,
            meter: setup.meter,
            universe: setup.universe,
            market_id: setup.market_id,
            first_seen: None,
            episode: None,
            closes: Vec::new(),
            liquidations: 0,
            cost_usd: 0.0,
        })
    }

    fn hawkeye(&self) -> PhoenixHawkeyeClient<'_> {
        self.exchange.hawkeye(&self.rpc_url, self.wallet.pubkey())
    }

    /// Every trader account the bot holds, found on chain: its
    /// cross-margin account first, then the rest in address order.
    async fn traders(&self) -> Result<Vec<Pubkey>> {
        let filters = vec![
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                0,
                PhoenixAccount::Trader.discriminant().to_vec(),
            )),
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                std::mem::offset_of!(TraderHeader, authority),
                self.owner.to_bytes().to_vec(),
            )),
        ];
        let mut others: Vec<Pubkey> = self
            .chain
            .get_program_accounts(&PHOENIX_PROGRAM_ID, filters)
            .await
            .map_err(|error| Error::Transaction {
                label: "list the bot's traders".into(),
                reason: error.to_string(),
            })?
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| *key != self.main_trader)
            .collect();
        others.sort();
        others.dedup();
        Ok([self.main_trader].into_iter().chain(others).collect())
    }

    /// The bot's value across its accounts, and its position in the market.
    async fn account(&self) -> Result<(f64, f64)> {
        let measured = patiently("the account", async || {
            self.meter.measure(&self.universe).await
        })
        .await?;
        Ok((
            grader_f64(measured.value_usd),
            crate::perp::position_in(&measured, &self.market_id),
        ))
    }

    /// One liquidation of `trader`, in whichever way its tier allows.
    /// `Ok(None)`: nothing to liquidate in the market. The outcome is
    /// `Err(reason)` when Phoenix refused it.
    async fn liquidate(
        &mut self,
        rpc: &RpcClient,
        step: &CrankStep,
        trader: Pubkey,
        tier: u8,
    ) -> Result<Option<std::result::Result<(), String>>> {
        let asset = patiently("the market's margin", async || {
            self.hawkeye()
                .view_margin_for_asset_for_trader(trader, self.market.asset_id)
                .await
        })
        .await?
        .value;
        if asset.base_lots == 0 {
            return Ok(None);
        }
        let long = asset.base_lots > 0;
        let held = asset.base_lots.unsigned_abs();
        let max = u64::from(self.market.max_liquidation_size_base_lots);
        let size = held.min(max);
        let mark = asset.mark_price_ticks;
        let through = (mark as f64 * LIQUIDATION_BAND) as u64;
        let limit_ticks = if long { mark - through } else { mark + through };
        let backstop = tier >= BACKSTOP_TIER;
        let (value_before, position_before) = self.account().await?;
        // Past the backstop margin the keeper takes the position over; if
        // Phoenix refuses that, or short of it, it closes through the book.
        let mut outcome: std::result::Result<String, String> = Err(String::new());
        let mut transferred = false;
        if backstop {
            let transfer = liquidation_transfer(
                &self.keys,
                self.wallet.pubkey(),
                self.wallet_trader,
                trader,
                u64::from(self.market.asset_id),
                size,
            );
            outcome = self.send(rpc, transfer, "liquidation transfer")?;
            transferred = outcome.is_ok();
        }
        if outcome.is_err() {
            let order = liquidate_via_market_order(
                &self.keys,
                self.wallet.pubkey(),
                trader,
                size,
                limit_ticks,
            );
            outcome = self.send(rpc, order.clone(), "liquidate")?;
            // Phoenix refuses to liquidate a trader with risk-increasing
            // orders resting: a keeper force-cancels them first, which
            // Phoenix allows any signer while the trader is liquidatable.
            // Without it, resting an order would make an account
            // unliquidatable.
            if outcome.is_err() {
                let cancel = force_cancel_risk_increasing(&self.keys, self.wallet.pubkey(), trader);
                if self.send(rpc, cancel, "force cancel")?.is_ok() {
                    outcome = self.send(rpc, order, "liquidate")?;
                }
            }
        }
        let ok = outcome.is_ok();
        let (value_after, position_after) = if ok {
            self.account().await?
        } else {
            (value_before, position_before)
        };
        let units = |lots: u64| lots as f64 / 10f64.powi(i32::from(self.market.base_lots_decimals));
        let order = CloseOrder {
            at: step.at,
            side: if long { "sell" } else { "buy" }.into(),
            size: units(size),
            limit_usd: mtx_kit::venue::ticks_to_usd(
                &self.market,
                if transferred { mark } else { limit_ticks },
            ),
            position_after,
            ok,
            error: outcome.clone().err(),
            signature: outcome.clone().ok(),
        };
        if ok {
            self.liquidations += 1;
            info!(
                step = step.k,
                trader = %trader,
                transferred,
                position_before,
                position_after,
                "the keeper liquidated"
            );
        }
        let episode = self.episode.get_or_insert_with(|| Episode {
            step: step.k,
            at: step.at,
            value_before_usd: value_before,
            position_before,
            value_after_usd: value_before,
            orders: Vec::new(),
            position_after: position_before,
        });
        if ok {
            episode.value_after_usd = value_after;
            episode.position_after = position_after;
        }
        episode.orders.push(order);
        Ok(Some(outcome.map(|_| ())))
    }

    /// Send one of the keeper's transactions: its signature, or `Err`
    /// inside for Phoenix refusing it, which the session goes on from.
    fn send(
        &self,
        rpc: &RpcClient,
        instruction: Instruction,
        label: &str,
    ) -> Result<std::result::Result<String, String>> {
        match mtx_kit::send(rpc, &self.wallet, vec![instruction], label) {
            Ok(signature) => Ok(Ok(signature.to_string())),
            Err(mtx_kit::Error::Transaction { reason, .. }) => Ok(Err(reason)),
            Err(error) => Err(error.into()),
        }
    }

    /// A turn. The final one (`settling`) doesn't wait out the reaction time.
    async fn turn(
        &mut self,
        rpc: &RpcClient,
        step: &CrankStep,
        settling: bool,
    ) -> Result<CrankOutcome> {
        if !settling && !step.k.is_multiple_of(self.spec.check_every_steps) {
            return Ok(CrankOutcome::Done);
        }
        let mut breached: Vec<(Pubkey, u8)> = Vec::new();
        for trader in self.traders().await? {
            let margin = patiently("the margin", async || {
                self.hawkeye().view_margin_for_trader(trader).await
            })
            .await?
            .value;
            if margin.is_liquidatable != 0 {
                breached.push((trader, margin.risk_tier));
            }
        }
        if breached.is_empty() {
            self.first_seen = None;
            self.close_episode();
            return Ok(CrankOutcome::Done);
        }
        let first = *self.first_seen.get_or_insert(step.at);
        if !settling && step.at.saturating_sub(first) < self.delay_slots {
            return Ok(CrankOutcome::Done);
        }
        let mut refused = None;
        for (trader, tier) in breached {
            if let Some(Err(reason)) = self.liquidate(rpc, step, trader, tier).await? {
                warn!(%trader, %reason, "Phoenix refused the keeper's liquidation");
                refused = Some(reason);
            }
        }
        Ok(refused.map_or(CrankOutcome::Done, CrankOutcome::Refused))
    }

    fn close_episode(&mut self) {
        let Some(episode) = self.episode.take() else {
            return;
        };
        // Only a stretch where a liquidation landed is a close.
        if !episode.orders.iter().any(|o| o.ok) {
            return;
        }
        self.cost_usd += episode.value_before_usd - episode.value_after_usd;
        self.closes.push(ForcedClose {
            reason: ForcedCloseReason::Liquidation {
                by: LiquidatedBy::Keeper,
            },
            step: episode.step,
            at: episode.at,
            value_before_usd: episode.value_before_usd,
            // The session fills in its running peak at the step.
            peak_usd: episode.value_before_usd,
            value_after_usd: episode.value_after_usd,
            position_before: episode.position_before,
            orders: episode.orders,
            flat: is_flat(episode.position_after),
        });
    }
}

/// `risk_tier` at which Phoenix reads an account past its backstop margin
/// (`RiskTier::BackstopLiquidatable`).
const BACKSTOP_TIER: u8 = 4;

#[async_trait]
impl VenueCrank for LiquidationKeeper {
    fn name(&self) -> &str {
        "phoenix liquidator"
    }

    async fn crank(&mut self, rpc: &RpcClient, step: &CrankStep) -> Result<CrankOutcome> {
        self.turn(rpc, step, false).await
    }

    /// One last turn after the session's last step, which doesn't wait out
    /// the reaction time: an account still liquidatable then is priced as
    /// the keeper would have, rather than ending the session liquidatable.
    async fn settle(&mut self, rpc: &RpcClient, step: &CrankStep) -> Result<CrankOutcome> {
        let outcome = self.turn(rpc, step, true).await?;
        self.close_episode();
        Ok(outcome)
    }

    fn forced_closes(&mut self) -> Vec<ForcedClose> {
        self.close_episode();
        std::mem::take(&mut self.closes)
    }

    fn liquidator_report(&self) -> Option<LiquidatorReport> {
        Some(LiquidatorReport {
            delay_slots: self.delay_slots,
            liquidations: self.liquidations,
            cost_usd: self.cost_usd,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys {
            global_config: Pubkey::new_unique(),
            perp_asset_map: Pubkey::new_unique(),
            global_trader_index: vec![Pubkey::new_unique()],
            active_trader_buffer: vec![Pubkey::new_unique()],
            mint: Pubkey::new_from_array(
                hex("05d08ca0437ee7562b6648e76d5094af350b57c3820b6716fc96e777d449a9dd")
                    .try_into()
                    .unwrap(),
            ),
            orderbook: Pubkey::new_unique(),
            splines: Pubkey::new_unique(),
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The data of a mainnet `LiquidateViaMarketOrder` (slot 429053028), as
    /// surfpool's Phoenix tests pin it.
    #[test]
    fn the_liquidation_encodes_like_a_mainnet_one() {
        let mainnet = "fbf1b86c46467fc605d08ca0437ee7562b6648e76d5094af350b57c3820b6716fc96e777d449a9dd\
                       951c0000000000004d4000000000000000";
        let ix = liquidate_via_market_order(
            &keys(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            7317,
            16461,
        );
        assert_eq!(ix.data, hex(mainnet));
    }

    /// Phoenix's IDL lists 11 accounts for a market-order liquidation, the
    /// liquidated trader and its account the same key, and the keeper the
    /// only signer.
    #[test]
    fn the_liquidation_names_phoenixs_accounts_in_order() {
        let (keeper, trader) = (Pubkey::new_unique(), Pubkey::new_unique());
        let k = keys();
        let ix = liquidate_via_market_order(&k, keeper, trader, 1, 1);
        let keys: Vec<Pubkey> = ix.accounts.iter().map(|a| a.pubkey).collect();
        assert_eq!(keys.len(), 11);
        assert_eq!(keys[0], *PHOENIX_PROGRAM_ID);
        assert_eq!(keys[3], keeper);
        assert_eq!((keys[4], keys[5]), (trader, trader));
        assert_eq!(keys[6], k.perp_asset_map);
        assert_eq!(keys[9], k.orderbook);
        let signers: Vec<usize> = (0..keys.len())
            .filter(|i| ix.accounts[*i].is_signer)
            .collect();
        assert_eq!(signers, [3]);
        assert!(ix.accounts[4].is_writable && !ix.accounts[3].is_writable);
    }

    #[test]
    fn discriminants_match_the_idl() {
        assert_eq!(
            PhoenixInstruction::LiquidateViaMarketOrder.discriminant(),
            [251, 241, 184, 108, 70, 70, 127, 198]
        );
        assert_eq!(
            PhoenixInstruction::LiquidationTransfer.discriminant(),
            [57, 218, 215, 68, 174, 23, 204, 127]
        );
        assert_eq!(
            PhoenixInstruction::ForceCancelRiskIncreasing.discriminant(),
            [41, 55, 217, 35, 92, 72, 42, 238]
        );
    }

    #[test]
    fn a_transfer_carries_one_request_for_all_of_it() {
        let ix = liquidation_transfer(
            &keys(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            39,
            1_490_000,
        );
        let tail = &ix.data[8..];
        assert_eq!(tail.len(), 4 + 8 + 8 + 1);
        assert_eq!(u32::from_le_bytes(tail[..4].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(tail[4..12].try_into().unwrap()), 39);
        assert_eq!(
            u64::from_le_bytes(tail[12..20].try_into().unwrap()),
            1_490_000
        );
        assert_eq!(tail[20], 1);
        assert_eq!(ix.accounts.len(), 9);
        assert!(ix.accounts[4].is_signer);
    }

    #[test]
    fn the_reaction_time_is_one_draw_from_the_seed() {
        let spec = Liquidator {
            reaction_slots: [2, 12],
            check_every_steps: 1,
        };
        let draws: Vec<u64> = (0u8..32).map(|s| spec.delay_slots([s; 32])).collect();
        assert!(draws.iter().all(|d| (2..=12).contains(d)));
        assert!(draws.iter().any(|d| *d != draws[0]), "the seed picks it");
        assert_eq!(spec.delay_slots([9; 32]), spec.delay_slots([9; 32]));
        let fixed = Liquidator {
            reaction_slots: [6, 6],
            check_every_steps: 1,
        };
        assert_eq!(fixed.delay_slots([1; 32]), 6);
    }

    #[test]
    fn a_backwards_range_or_no_cadence_is_refused() {
        let ok = Liquidator {
            reaction_slots: [2, 12],
            check_every_steps: 1,
        };
        assert!(ok.validate().is_ok());
        assert!(
            Liquidator {
                reaction_slots: [12, 2],
                ..ok
            }
            .validate()
            .is_err()
        );
        assert!(
            Liquidator {
                check_every_steps: 0,
                ..ok
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn the_spec_reads_with_its_default_cadence() {
        let spec: Liquidator = serde_json::from_str(r#"{"reaction_slots":[2,12]}"#).unwrap();
        assert_eq!(spec.check_every_steps, 1);
        assert!(serde_json::from_str::<Liquidator>(r#"{"reaction_slots":[2,12],"x":1}"#).is_err());
    }
}

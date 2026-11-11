//! [`OpenBookV2Meter`]: what an enforcer measures of an account on OpenBook
//! v2's markets, as the OpenBook venue profile defines it.

use std::sync::Arc;

use solana_pubkey::Pubkey;

use super::{
    OpenBookV2Market,
    fills::{heap_fills, logged_fills},
    ix,
    state::{OpenOrdersIndexer, OpenOrdersPosition},
};
use crate::{
    chain::{ChainClient, ChainError, token},
    core::{AccountMeter, InstrumentId, Measurement, MeterError, RecordedFill, SpotBalances},
    protocols::{
        NamedMarket, holdings_from_atoms,
        spot::{MarketFill, meter_error, recorded_for, value_usd},
    },
};

/// What `owner` holds on `market`: every token account it owns for the
/// market's two mints, and every open orders account its indexer lists on
/// the market, summed. Free is what those accounts may settle; locked is
/// what their resting orders hold, asks the base and bids the quote with
/// their maker fees. A maker's fills count once `consume_events` has
/// settled them. For SOL, the wallet counts wrapped SOL only.
pub async fn balances(
    chain: &dyn ChainClient,
    market: &OpenBookV2Market,
    owner: &Pubkey,
) -> Result<SpotBalances, ChainError> {
    let k = &market.keys;
    let listed = match chain.get_account(&ix::open_orders_indexer(owner)).await? {
        Some(account) => OpenOrdersIndexer::decode(&account.data)?.addresses,
        None => Vec::new(),
    };
    let open_orders = if listed.is_empty() {
        Vec::new()
    } else {
        chain.get_multiple_accounts(&listed).await?
    };
    let a = &market.account;
    let (mut base_free, mut base_locked, mut quote_free, mut quote_locked) = (0, 0, 0, 0);
    for account in open_orders.into_iter().flatten() {
        let p = OpenOrdersPosition::decode(&account.data)?;
        if p.market == k.market && &p.owner == owner {
            base_free += i128::from(p.base_free_native);
            base_locked += i128::from(p.asks_base_lots) * i128::from(a.base_lot_size);
            quote_free += i128::from(p.quote_free_native);
            quote_locked += i128::from(p.bids_quote_lots) * i128::from(a.quote_lot_size)
                + i128::from(p.locked_maker_fees);
        }
    }
    let i = &market.instrument;
    Ok(SpotBalances {
        base: holdings_from_atoms(
            &i.base,
            token::owned_amount(chain, owner, &k.base_mint)
                .await?
                .into(),
            base_free,
            base_locked,
        )?,
        quote: holdings_from_atoms(
            &i.quote,
            token::owned_amount(chain, owner, &k.quote_mint)
                .await?
                .into(),
            quote_free,
            quote_locked,
        )?,
    })
}

/// One owner's account on a set of OpenBook v2 markets: those in a
/// mandate's universe, and any others it should watch for the universe
/// rule. Reads through its own [`ChainClient`], never the bot's.
pub struct OpenBookV2Meter {
    chain: Arc<dyn ChainClient>,
    owner: Pubkey,
    markets: Vec<NamedMarket>,
}

impl OpenBookV2Meter {
    /// Measures `owner` on `markets`, such as [`super::MARKETS`].
    pub fn new(chain: Arc<dyn ChainClient>, owner: Pubkey, markets: &[NamedMarket]) -> Self {
        Self {
            chain,
            owner,
            markets: markets.to_vec(),
        }
    }

    /// The owner the meter measures.
    pub fn owner(&self) -> Pubkey {
        self.owner
    }

    /// Who the owner is in a fill: itself (the taker of a take-only order)
    /// and every open orders account its indexer lists.
    pub async fn fill_ids(&self) -> Result<Vec<Pubkey>, MeterError> {
        let mut ids = vec![self.owner];
        if let Some(account) = self
            .chain
            .get_account(&ix::open_orders_indexer(&self.owner))
            .await
            .map_err(meter_error)?
        {
            ids.extend(
                OpenOrdersIndexer::decode(&account.data)
                    .map_err(meter_error)?
                    .addresses,
            );
        }
        Ok(ids)
    }

    /// Every fill the watched markets recorded, between any accounts,
    /// oldest first: the `FillLog`s of transactions that touched each market
    /// in slots after `after_slot`, then the `FillEvent`s still in its event
    /// heap, which no transaction has logged yet, dated the slot they were
    /// read at. A fill stays in the heap, and in every read, until
    /// `consume_events` takes it out: count each `sequence` once.
    pub async fn market_fills(&self, after_slot: u64) -> Result<Vec<MarketFill>, MeterError> {
        let chain = self.chain.as_ref();
        let mut fills = Vec::new();
        for named in &self.markets {
            let market = self.market(named).await.map_err(meter_error)?;
            let signatures = chain
                .signatures_for_address(&named.address, after_slot)
                .await
                .map_err(meter_error)?;
            let mut logged = Vec::new();
            for (signature, _) in signatures {
                let summary = chain
                    .get_transaction(&signature)
                    .await
                    .map_err(meter_error)?
                    .ok_or_else(|| {
                        MeterError::Read(format!("transaction {signature} is listed but not found"))
                    })?;
                logged.extend(logged_fills(&summary, &market));
            }
            let slot = chain.get_slot().await.map_err(meter_error)?;
            let heap = chain
                .get_account(&market.keys.event_heap)
                .await
                .map_err(meter_error)?
                .ok_or_else(|| meter_error(ChainError::AccountNotFound(market.keys.event_heap)))?;
            let waiting = heap_fills(&heap.data, &market, slot).map_err(meter_error)?;
            let seen: Vec<u128> = logged.iter().map(|fill| fill.sequence).collect();
            fills.extend(logged);
            fills.extend(waiting.into_iter().filter(|f| !seen.contains(&f.sequence)));
        }
        fills.sort_by_key(|fill| (fill.slot, fill.sequence));
        Ok(fills)
    }

    async fn market(&self, named: &NamedMarket) -> Result<OpenBookV2Market, ChainError> {
        let account = self
            .chain
            .get_account(&named.address)
            .await?
            .ok_or(ChainError::AccountNotFound(named.address))?;
        let (base, quote) = named
            .symbol
            .split_once('-')
            .unwrap_or((named.symbol, "USDC"));
        OpenBookV2Market::decode(named.address, &account.data, base, quote).map_err(|e| {
            ChainError::Decode {
                what: "openbook v2 market",
                detail: e.to_string(),
            }
        })
    }
}

#[async_trait::async_trait]
impl AccountMeter for OpenBookV2Meter {
    /// Every watched market's holding, flat ones included. A market outside
    /// `universe` counts against it when an open orders account has orders
    /// resting there. Holdings of an asset count wherever they sit.
    async fn measure(&self, universe: &[InstrumentId]) -> Result<Measurement, MeterError> {
        let chain = self.chain.as_ref();
        let mut holdings = Vec::with_capacity(self.markets.len());
        let mut all = Vec::with_capacity(self.markets.len());
        let mut outside_universe = 0;
        for named in &self.markets {
            let market = self.market(named).await.map_err(meter_error)?;
            let held = balances(chain, &market, &self.owner)
                .await
                .map_err(meter_error)?;
            if held.has_orders() && !universe.contains(market.id()) {
                outside_universe += 1;
            }
            holdings.push((market.id().clone(), held.holding()));
            all.push(held);
        }
        Ok(Measurement {
            holdings,
            value_usd: value_usd(chain, &all).await?,
            outside_universe,
        })
    }

    /// The owner's fills, as a taker or a maker, settled or still in the
    /// event heap (see [`OpenBookV2Meter::market_fills`]).
    async fn fills(&self, after_slot: u64) -> Result<Vec<RecordedFill>, MeterError> {
        let fills = self.market_fills(after_slot).await?;
        Ok(recorded_for(&fills, &self.fill_ids().await?))
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use solana_account::Account;

    use super::*;
    use crate::{
        chain::{TransactionSummary, mock::MockChainClient},
        core::Side,
        protocols::{
            USDC_MINT, WRAPPED_SOL_MINT,
            openbook::v2::{
                PROGRAM_ID,
                fills::testing::{EventSpec, fill_log_line, heap},
                state::testing::{indexer_bytes, market_bytes, open_orders_bytes},
            },
            spot::testing::{sol_usd, token_account},
        },
    };

    #[tokio::test]
    async fn sums_the_open_orders_accounts_and_values_both_assets() {
        let chain = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let mut keys: [Pubkey; 8] =
            std::array::from_fn(|i| Pubkey::new_from_array([i as u8 + 1; 32]));
        (keys[4], keys[5]) = (WRAPPED_SOL_MINT, USDC_MINT);
        let named = crate::protocols::openbook::v2::MARKETS[0];
        let address = named.address;
        let program = |data| Account {
            lamports: 1,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };
        chain.set_account(address, program(market_bytes(&keys)));
        let (first, second) = (Pubkey::new_unique(), Pubkey::new_unique());
        chain.set_account(
            ix::open_orders_indexer(&owner),
            program(indexer_bytes(&[first, second])),
        );
        // 2 SOL locked behind asks and 1 free; 4 USDC behind bids, 1 free.
        chain.set_account(
            first,
            program(open_orders_bytes(
                &owner,
                &address,
                2_000,
                1_000_000_000,
                4_000_000,
                0,
            )),
        );
        chain.set_account(
            second,
            program(open_orders_bytes(&owner, &address, 0, 0, 0, 1_000_000)),
        );
        token_account(
            &chain,
            Pubkey::new_unique(),
            owner,
            WRAPPED_SOL_MINT,
            500_000_000,
        );
        sol_usd(&chain, 80_00000000);

        let m = OpenBookV2Meter::new(chain, owner, &[named])
            .measure(&[])
            .await
            .unwrap();
        assert_eq!(m.holdings[0].0, InstrumentId::new("openbook:spot:SOL-USDC"));
        assert_eq!(m.holdings[0].1.position(), dec!(3.5));
        // 3.5 SOL at $80, and 5 USDC.
        assert_eq!(m.value_usd, dec!(285));
        assert_eq!(m.outside_universe, 1);
    }

    #[tokio::test]
    async fn fills_come_from_the_event_heap_and_the_fill_logs_once_each() {
        let chain = Arc::new(MockChainClient::new());
        chain.set_slot(50);
        let owner = Pubkey::new_unique();
        let open_orders = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let mut keys: [Pubkey; 8] =
            std::array::from_fn(|i| Pubkey::new_from_array([i as u8 + 1; 32]));
        (keys[4], keys[5]) = (WRAPPED_SOL_MINT, USDC_MINT);
        let named = crate::protocols::openbook::v2::MARKETS[0];
        let program = |data| Account {
            lamports: 1,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };
        chain.set_account(named.address, program(market_bytes(&keys)));
        chain.set_account(
            ix::open_orders_indexer(&owner),
            program(indexer_bytes(&[open_orders])),
        );
        // The owner sold 2 lots as a take-only taker (seq 1, logged and still
        // in the heap); another taker bought from the owner's ask (seq 2,
        // only in the heap); two others traded (seq 3).
        let sold = EventSpec {
            taker_side: 1,
            seq_num: 1,
            maker: other,
            taker: owner,
            price_lots: 150,
            base_lots: 2,
        };
        let lifted = EventSpec {
            taker_side: 0,
            seq_num: 2,
            maker: open_orders,
            taker: other,
            price_lots: 151,
            base_lots: 3,
        };
        let theirs = EventSpec {
            taker_side: 0,
            seq_num: 3,
            maker: other,
            taker: Pubkey::new_unique(),
            price_lots: 152,
            base_lots: 1,
        };
        let line = fill_log_line(&named.address, &sold);
        chain.set_account(keys[3], program(heap(&[sold, lifted, theirs])));
        chain.record_transaction(
            TransactionSummary {
                signature: solana_transaction::Signature::from([1; 64]),
                slot: 40,
                block_time: None,
                fee_lamports: 5_000,
                error: None,
                logs: vec!["Program log: Instruction: PlaceTakeOrder".into(), line],
                account_keys: Vec::new(),
                pre_balances: Vec::new(),
                post_balances: Vec::new(),
                token_balance_changes: Vec::new(),
                inner_instructions: Vec::new(),
            },
            &[named.address],
        );

        let meter = OpenBookV2Meter::new(chain, owner, &[named]);
        let all = meter.market_fills(0).await.unwrap();
        let seen: Vec<(u64, u128)> = all.iter().map(|f| (f.slot, f.sequence)).collect();
        assert_eq!(seen, vec![(40, 1), (50, 2), (50, 3)]);
        assert_eq!(all[0].price, dec!(0.15));
        assert_eq!(all[0].quantity, dec!(0.002));

        let mine = meter.fills(0).await.unwrap();
        let sides: Vec<Side> = mine.iter().map(|f| f.fill.side).collect();
        assert_eq!(sides, vec![Side::Sell, Side::Sell]);
    }
}

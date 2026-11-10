//! [`OpenBookV2Meter`]: what an enforcer measures of an account on OpenBook
//! v2's markets, as the OpenBook venue profile defines it.

use std::sync::Arc;

use solana_pubkey::Pubkey;

use super::{
    OpenBookV2Market, ix,
    state::{OpenOrdersIndexer, OpenOrdersPosition},
};
use crate::{
    chain::{ChainClient, ChainError, token},
    core::{AccountMeter, InstrumentId, Measurement, MeterError, RecordedFill, SpotBalances},
    protocols::{
        NamedMarket, holdings_from_atoms,
        spot::{meter_error, value_usd},
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

    async fn fills(&self, _after_slot: u64) -> Result<Vec<RecordedFill>, MeterError> {
        Err(MeterError::Unsupported(
            "OpenBook v2 fills: FillLog and the event heap aren't decoded yet".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use solana_account::Account;

    use super::*;
    use crate::{
        chain::mock::MockChainClient,
        protocols::{
            USDC_MINT, WRAPPED_SOL_MINT,
            openbook::v2::{
                PROGRAM_ID,
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
}

//! [`PhoenixV1Meter`]: what an enforcer measures of an account on Phoenix
//! v1's spot markets, as the Phoenix venue profile (Spot markets) defines
//! it.

use std::sync::Arc;

use solana_pubkey::Pubkey;

use super::{PhoenixV1Market, state::MarketState};
use crate::{
    chain::{ChainClient, ChainError, token},
    core::{AccountMeter, InstrumentId, Measurement, MeterError, RecordedFill, SpotBalances},
    protocols::{
        NamedMarket, holdings_from_atoms,
        spot::{meter_error, value_usd},
    },
};

/// What `owner` holds on `market`: every token account it owns for the
/// market's two mints, and its seat, free and locked. For SOL, the wallet
/// counts wrapped SOL only; native SOL pays fees.
pub async fn balances(
    chain: &dyn ChainClient,
    market: &PhoenixV1Market,
    state: &MarketState,
    owner: &Pubkey,
) -> Result<SpotBalances, ChainError> {
    let k = &market.keys;
    let h = &state.header;
    let seat = state.seat(owner).copied().unwrap_or_default();
    let atoms = |lots: u64, lot_size: u64| i128::from(lots) * i128::from(lot_size);
    let i = &market.instrument;
    Ok(SpotBalances {
        base: holdings_from_atoms(
            &i.base,
            token::owned_amount(chain, owner, &k.base_mint)
                .await?
                .into(),
            atoms(seat.base_lots_free, h.base_lot_size),
            atoms(seat.base_lots_locked, h.base_lot_size),
        )?,
        quote: holdings_from_atoms(
            &i.quote,
            token::owned_amount(chain, owner, &k.quote_mint)
                .await?
                .into(),
            atoms(seat.quote_lots_free, h.quote_lot_size),
            atoms(seat.quote_lots_locked, h.quote_lot_size),
        )?,
    })
}

/// One owner's account on a set of Phoenix v1 markets: those in a
/// mandate's universe, and any others it should watch for the universe
/// rule. Reads through its own [`ChainClient`], never the bot's.
pub struct PhoenixV1Meter {
    chain: Arc<dyn ChainClient>,
    owner: Pubkey,
    markets: Vec<NamedMarket>,
}

impl PhoenixV1Meter {
    /// Measures `owner` on `markets`, such as [`super::MARKETS`].
    pub fn new(chain: Arc<dyn ChainClient>, owner: Pubkey, markets: &[NamedMarket]) -> Self {
        Self {
            chain,
            owner,
            markets: markets.to_vec(),
        }
    }

    async fn market(
        &self,
        named: &NamedMarket,
    ) -> Result<(PhoenixV1Market, MarketState), ChainError> {
        let account = self
            .chain
            .get_account(&named.address)
            .await?
            .ok_or(ChainError::AccountNotFound(named.address))?;
        let (base, quote) = named
            .symbol
            .split_once('-')
            .unwrap_or((named.symbol, "USDC"));
        let market =
            PhoenixV1Market::decode(named.address, &account.data, base, quote).map_err(|e| {
                ChainError::Decode {
                    what: "phoenix v1 market",
                    detail: e.to_string(),
                }
            })?;
        Ok((market, MarketState::decode(&account.data)?))
    }
}

#[async_trait::async_trait]
impl AccountMeter for PhoenixV1Meter {
    /// Every watched market's holding, flat ones included. A market outside
    /// `universe` counts against it when the seat has orders resting there.
    /// Holdings of an asset count wherever they sit: a spot position
    /// belongs to the asset, not to a market.
    async fn measure(&self, universe: &[InstrumentId]) -> Result<Measurement, MeterError> {
        let chain = self.chain.as_ref();
        let mut holdings = Vec::with_capacity(self.markets.len());
        let mut all = Vec::with_capacity(self.markets.len());
        let mut outside_universe = 0;
        for named in &self.markets {
            let (market, state) = self.market(named).await.map_err(meter_error)?;
            let held = balances(chain, &market, &state, &self.owner)
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
            "Phoenix v1 fills: the audit log's Fill events aren't decoded yet".into(),
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
        core::Holding,
        protocols::{
            USDC_MINT, WRAPPED_SOL_MINT,
            phoenix::v1::{
                PROGRAM_ID,
                state::testing::{MarketSpec, market_bytes},
            },
            spot::testing::{sol_usd, token_account},
        },
    };

    #[tokio::test]
    async fn measures_seat_and_every_token_account_at_the_oracle() {
        let chain = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let named = crate::protocols::phoenix::v1::MARKETS[0];
        // 1 SOL free and 2 locked behind asks; 5 USDC free on the seat.
        let data = market_bytes(&MarketSpec {
            base_mint: WRAPPED_SOL_MINT,
            quote_mint: USDC_MINT,
            seats: vec![(owner, 1_000, 2_000, 5_000_000, 0)],
            ..MarketSpec::default()
        });
        chain.set_account(
            named.address,
            Account {
                lamports: 1,
                data,
                owner: PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        );
        // Half a SOL in the associated account, a quarter moved out of it.
        token_account(
            &chain,
            token::ata(&owner, &WRAPPED_SOL_MINT),
            owner,
            WRAPPED_SOL_MINT,
            500_000_000,
        );
        token_account(
            &chain,
            Pubkey::new_unique(),
            owner,
            WRAPPED_SOL_MINT,
            250_000_000,
        );
        token_account(
            &chain,
            token::ata(&owner, &USDC_MINT),
            owner,
            USDC_MINT,
            10_000_000,
        );
        // Someone else's SOL doesn't count.
        token_account(
            &chain,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            WRAPPED_SOL_MINT,
            9,
        );
        sol_usd(&chain, 100_00000000);

        let meter = PhoenixV1Meter::new(chain.clone(), owner, &[named]);
        let id = InstrumentId::new("phoenix:spot:SOL-USDC");
        let inside = meter.measure(std::slice::from_ref(&id)).await.unwrap();
        assert_eq!(inside.holdings.len(), 1);
        assert_eq!(inside.holdings[0].0, id);
        let Holding::Spot(base) = &inside.holdings[0].1 else {
            panic!("a spot holding");
        };
        assert_eq!(
            (base.wallet, base.free, base.locked),
            (dec!(0.75), dec!(1), dec!(2))
        );
        assert_eq!(inside.holdings[0].1.position(), dec!(3.75));
        // 3.75 SOL at $100, and 15 USDC.
        assert_eq!(inside.value_usd, dec!(390));
        assert_eq!(inside.outside_universe, 0);
        // Asks rest there, so the market counts against an empty universe.
        assert_eq!(meter.measure(&[]).await.unwrap().outside_universe, 1);
        assert!(meter.fills(0).await.is_err());
    }
}

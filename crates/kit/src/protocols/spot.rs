//! What the spot venues share: how the spot profile (`specs/instruments/spot.md`)
//! values an account's holdings.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use solana_pubkey::Pubkey;

use crate::{
    chain::{ChainClient, ChainError},
    core::{AssetId, MeterError, SpotBalances},
    protocols::{USDC_MINT, pyth},
};

/// What `balances`, one per market, are worth in USD. Each asset counts
/// once: its wallet once, however many markets trade it, plus what each
/// market holds of it, free or locked. Base assets are valued at their Pyth
/// price (see [`pyth::feed_for_mint`]); USDC at $1.
pub async fn value_usd(
    chain: &dyn ChainClient,
    balances: &[SpotBalances],
) -> Result<Decimal, MeterError> {
    let mut held: BTreeMap<AssetId, (Decimal, Decimal)> = BTreeMap::new();
    for market in balances {
        for asset in [&market.base, &market.quote] {
            let (wallet, on_venue) = held.entry(asset.base.id.clone()).or_default();
            *wallet = asset.wallet;
            *on_venue += asset.free + asset.locked;
        }
    }
    let mut value = Decimal::ZERO;
    for (id, (wallet, on_venue)) in held {
        let mint: Pubkey = id.0.parse().map_err(|_| {
            MeterError::Unsupported(format!(
                "asset {id} is not a mint, so it has no reference price"
            ))
        })?;
        let price = if mint == USDC_MINT {
            Decimal::ONE
        } else {
            let feed = pyth::feed_for_mint(&mint)
                .ok_or_else(|| MeterError::Unsupported(format!("no reference price for {mint}")))?;
            pyth::read_price(chain, &feed).await.map_err(meter_error)?
        };
        value += (wallet + on_venue) * price;
    }
    Ok(value)
}

/// A chain error as a meter's: a failed read, or an account that did not
/// decode.
pub fn meter_error(error: ChainError) -> MeterError {
    match error {
        ChainError::Decode { what, detail } => MeterError::Decode { what, detail },
        other => MeterError::Read(other.to_string()),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Accounts a spot meter reads, for tests: token accounts and a Pyth
    //! price.

    use solana_account::Account;
    use solana_program_pack::Pack;
    use solana_pubkey::Pubkey;
    use spl_token_interface::state::{Account as TokenState, AccountState};

    use crate::{
        chain::{mock::MockChainClient, token},
        protocols::pyth,
    };

    /// A token account at `address` holding `amount` atoms of `mint` for
    /// `owner`.
    pub(crate) fn token_account(
        chain: &MockChainClient,
        address: Pubkey,
        owner: Pubkey,
        mint: Pubkey,
        amount: u64,
    ) {
        let mut data = vec![0u8; TokenState::LEN];
        TokenState {
            mint,
            owner,
            amount,
            state: AccountState::Initialized,
            ..Default::default()
        }
        .pack_into_slice(&mut data);
        chain.set_account(
            address,
            Account {
                lamports: 1,
                data,
                owner: token::token_program(),
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    /// The SOL/USD feed's account, at `price_e8` × 10^-8.
    pub(crate) fn sol_usd(chain: &MockChainClient, price_e8: i64) {
        let mut d = pyth::PRICE_UPDATE_V2_DISCRIMINATOR.to_vec();
        d.extend_from_slice(&[7; 32]);
        d.push(1);
        d.extend_from_slice(&pyth::SOL_USD_FEED_ID);
        d.extend_from_slice(&price_e8.to_le_bytes());
        d.extend_from_slice(&1_000u64.to_le_bytes());
        d.extend_from_slice(&(-8i32).to_le_bytes());
        for _ in 0..3 {
            d.extend_from_slice(&1_700_000_000i64.to_le_bytes());
        }
        d.extend_from_slice(&1_000u64.to_le_bytes());
        d.extend_from_slice(&1u64.to_le_bytes());
        chain.set_account(
            pyth::feed_account(0, &pyth::SOL_USD_FEED_ID),
            Account {
                lamports: 1,
                data: d,
                owner: pyth::PUSH_ORACLE_ID,
                executable: false,
                rent_epoch: 0,
            },
        );
    }
}

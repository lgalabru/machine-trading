use std::{path::PathBuf, str::FromStr};

use luce::{
    Decimal, core::RiskLimits, protocols::phoenix::phoenix_rise::types::exchange::ExchangeResponse,
};
use solana_keypair::Keypair;

use crate::{Error, Result, error::io_error, phoenix::load_exchange};

/// What the grader hands a bot, through environment variables.
#[derive(Debug, Clone)]
pub struct BotEnv {
    /// `OTT_RPC_URL`: HTTP JSON-RPC endpoint. Standard Solana methods only.
    pub rpc_url: String,
    /// `OTT_KEYPAIR`: Solana CLI keypair file (JSON array of 64 bytes). Holds
    /// SOL for fees; its Phoenix trader account (PDA index 0, subaccount 0)
    /// is registered and holds the trading collateral.
    pub keypair_path: PathBuf,
    /// `OTT_EXCHANGE`: Phoenix `/exchange` metadata matching the chain state.
    pub exchange_path: PathBuf,
    /// `OTT_SYMBOL`: the market to trade, e.g. `SOL`.
    pub symbol: String,
    /// `OTT_END_SLOT`: the slot at which the session ends and the account is
    /// marked to market. The bot is stopped shortly after.
    pub end_slot: u64,
    /// `OTT_MAX_POSITION`: the largest position, long or short, in base units
    /// (e.g. SOL), the bot may ever hold. Holding more disqualifies it.
    pub max_position: f64,
    /// `OTT_MAX_DRAWDOWN`: the most, in USD, the account's value may fall
    /// below its peak during a session. Falling further disqualifies it.
    pub max_drawdown_usd: f64,
}

impl BotEnv {
    pub const RPC_URL: &str = "OTT_RPC_URL";
    pub const KEYPAIR: &str = "OTT_KEYPAIR";
    pub const EXCHANGE: &str = "OTT_EXCHANGE";
    pub const SYMBOL: &str = "OTT_SYMBOL";
    pub const END_SLOT: &str = "OTT_END_SLOT";
    pub const MAX_POSITION: &str = "OTT_MAX_POSITION";
    pub const MAX_DRAWDOWN: &str = "OTT_MAX_DRAWDOWN";

    pub fn from_env() -> Result<Self> {
        let end_slot = var(Self::END_SLOT)?;
        let max_position = var(Self::MAX_POSITION)?;
        let max_drawdown = var(Self::MAX_DRAWDOWN)?;
        Ok(Self {
            rpc_url: var(Self::RPC_URL)?,
            keypair_path: var(Self::KEYPAIR)?.into(),
            exchange_path: var(Self::EXCHANGE)?.into(),
            symbol: var(Self::SYMBOL)?,
            end_slot: end_slot.parse().map_err(|_| Error::Env {
                name: Self::END_SLOT,
                reason: format!("not a slot number: {end_slot}"),
            })?,
            max_position: max_position.parse().map_err(|_| Error::Env {
                name: Self::MAX_POSITION,
                reason: format!("not a number: {max_position}"),
            })?,
            max_drawdown_usd: max_drawdown.parse().map_err(|_| Error::Env {
                name: Self::MAX_DRAWDOWN,
                reason: format!("not a number: {max_drawdown}"),
            })?,
        })
    }

    /// The variables to set on a bot process for this environment.
    pub fn vars(&self) -> [(&'static str, String); 7] {
        [
            (Self::RPC_URL, self.rpc_url.clone()),
            (Self::KEYPAIR, self.keypair_path.display().to_string()),
            (Self::EXCHANGE, self.exchange_path.display().to_string()),
            (Self::SYMBOL, self.symbol.clone()),
            (Self::END_SLOT, self.end_slot.to_string()),
            (Self::MAX_POSITION, self.max_position.to_string()),
            (Self::MAX_DRAWDOWN, self.max_drawdown_usd.to_string()),
        ]
    }

    pub fn keypair(&self) -> Result<Keypair> {
        let contents =
            std::fs::read_to_string(&self.keypair_path).map_err(io_error(&self.keypair_path))?;
        let bytes: Vec<u8> = serde_json::from_str(&contents)?;
        Keypair::try_from(bytes.as_slice()).map_err(|_| Error::Keypair(self.keypair_path.clone()))
    }

    pub fn exchange(&self) -> Result<ExchangeResponse> {
        load_exchange(&self.exchange_path)
    }

    /// The session's rules as luce [`RiskLimits`]: `OTT_MAX_POSITION` as the
    /// largest position in base units and `OTT_MAX_DRAWDOWN` as the largest
    /// fall from peak equity in USD. Every fraction-of-capital limit is off.
    ///
    /// Evaluate an order with [`RiskLimits::evaluate`] before sending it; a
    /// rejection means the grader would disqualify the bot.
    pub fn risk_limits(&self) -> Result<RiskLimits> {
        let decimal = |name: &'static str, value: f64| {
            Decimal::from_str(&format!("{value}")).map_err(|_| Error::Env {
                name,
                reason: format!("not a finite decimal: {value}"),
            })
        };
        Ok(RiskLimits::absolute(
            Some(decimal(Self::MAX_POSITION, self.max_position)?),
            Some(decimal(Self::MAX_DRAWDOWN, self.max_drawdown_usd)?),
        ))
    }
}

fn var(name: &'static str) -> Result<String> {
    std::env::var(name).map_err(|e| Error::Env {
        name,
        reason: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_limits_carry_the_session_rules() {
        let env = BotEnv {
            rpc_url: "http://localhost:8899".into(),
            keypair_path: "/dev/null".into(),
            exchange_path: "/dev/null".into(),
            symbol: "SOL".into(),
            end_slot: 1,
            max_position: 50.5,
            max_drawdown_usd: 500.0,
        };
        let limits = env.risk_limits().unwrap();
        assert_eq!(limits.max_position_quantity, Some(Decimal::new(505, 1)));
        assert_eq!(limits.max_drawdown_value, Some(Decimal::new(500, 0)));
        assert_eq!(limits.max_position_pct, RiskLimits::OFF);

        let bad = BotEnv {
            max_position: f64::NAN,
            ..env
        };
        assert!(matches!(
            bad.risk_limits().unwrap_err(),
            Error::Env {
                name: BotEnv::MAX_POSITION,
                ..
            }
        ));
    }
}

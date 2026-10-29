use std::{path::PathBuf, str::FromStr};

use luce::{
    Decimal, core::RiskLimits, protocols::phoenix::phoenix_rise::types::exchange::ExchangeResponse,
};
use solana_keypair::Keypair;

use crate::{
    Error, Result,
    error::io_error,
    mandate::{Mandate, Term},
    phoenix::load_exchange,
};

/// What the grader hands a bot, through environment variables.
#[derive(Debug, Clone)]
pub struct BotEnv {
    /// `MTH_RPC_URL`: HTTP JSON-RPC endpoint. Standard Solana methods only.
    pub rpc_url: String,
    /// `MTH_KEYPAIR`: Solana CLI keypair file (JSON array of 64 bytes). Holds
    /// SOL for fees; its Phoenix trader account (PDA index 0, subaccount 0)
    /// is registered and holds the trading collateral.
    pub keypair_path: PathBuf,
    /// `MTH_EXCHANGE`: Phoenix `/exchange` metadata matching the chain state.
    pub exchange_path: PathBuf,
    /// `MTH_SYMBOL`: the market to trade, e.g. `SPCX`.
    pub symbol: String,
    /// `MTH_END_SLOT`: the slot at which the session ends and the account is
    /// marked to market. The bot is stopped shortly after.
    pub end_slot: u64,
    /// `MTH_MAX_POSITION`: the largest position, long or short, in base units
    /// (e.g. SPCX shares), the bot may ever hold. Holding more disqualifies it.
    pub max_position: f64,
    /// `MTH_MAX_DRAWDOWN`: the most, in USD, the account's value may fall
    /// below its peak during a session. Falling further disqualifies it.
    pub max_drawdown_usd: f64,
    /// `MTH_MANDATE`: a JSON file with the session's [`Mandate`], the
    /// instruments the bot may hold and its limits. The variables above
    /// repeat its values. Optional, for graders that predate it.
    pub mandate_path: Option<PathBuf>,
}

impl BotEnv {
    pub const RPC_URL: &str = "MTH_RPC_URL";
    pub const KEYPAIR: &str = "MTH_KEYPAIR";
    pub const EXCHANGE: &str = "MTH_EXCHANGE";
    pub const SYMBOL: &str = "MTH_SYMBOL";
    pub const END_SLOT: &str = "MTH_END_SLOT";
    pub const MAX_POSITION: &str = "MTH_MAX_POSITION";
    pub const MAX_DRAWDOWN: &str = "MTH_MAX_DRAWDOWN";
    pub const MANDATE: &str = "MTH_MANDATE";

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
            mandate_path: std::env::var_os(Self::MANDATE).map(PathBuf::from),
        })
    }

    /// The variables to set on a bot process for this environment.
    pub fn vars(&self) -> Vec<(&'static str, String)> {
        let mut vars = vec![
            (Self::RPC_URL, self.rpc_url.clone()),
            (Self::KEYPAIR, self.keypair_path.display().to_string()),
            (Self::EXCHANGE, self.exchange_path.display().to_string()),
            (Self::SYMBOL, self.symbol.clone()),
            (Self::END_SLOT, self.end_slot.to_string()),
            (Self::MAX_POSITION, self.max_position.to_string()),
            (Self::MAX_DRAWDOWN, self.max_drawdown_usd.to_string()),
        ];
        if let Some(path) = &self.mandate_path {
            vars.push((Self::MANDATE, path.display().to_string()));
        }
        vars
    }

    /// The session's mandate: `MTH_MANDATE` when the grader sets it,
    /// otherwise the one the other variables describe, a single Phoenix
    /// perpetual.
    pub fn mandate(&self) -> Result<Mandate> {
        if let Some(path) = &self.mandate_path {
            return Mandate::load(path);
        }
        let mut mandate = Mandate::perp(
            "phoenix",
            &self.symbol,
            self.max_position,
            self.max_drawdown_usd,
        );
        mandate.term = Some(Term {
            until_slot: self.end_slot,
        });
        Ok(mandate)
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

    /// The session's rules as luce [`RiskLimits`]: `MTH_MAX_POSITION` as the
    /// largest position in base units and `MTH_MAX_DRAWDOWN` as the largest
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
            symbol: "SPCX".into(),
            end_slot: 1,
            max_position: 50.5,
            max_drawdown_usd: 500.0,
            mandate_path: None,
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

    #[test]
    fn without_a_mandate_file_the_variables_describe_one() {
        let env = BotEnv {
            rpc_url: "http://localhost:8899".into(),
            keypair_path: "/dev/null".into(),
            exchange_path: "/dev/null".into(),
            symbol: "SPCX".into(),
            end_slot: 480,
            max_position: 70.0,
            max_drawdown_usd: 1_000.0,
            mandate_path: None,
        };
        let mandate = env.mandate().unwrap();
        assert_eq!(mandate.term, Some(Term { until_slot: 480 }));
        assert_eq!(
            mandate.universe[0].instrument.to_string(),
            "phoenix:perp:SPCX"
        );
        assert!(!env.vars().iter().any(|(name, _)| *name == BotEnv::MANDATE));

        let dir = std::env::temp_dir().join(format!("mth-mandate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mandate.json");
        mandate.save(&path).unwrap();
        let from_file = BotEnv {
            mandate_path: Some(path.clone()),
            ..env
        };
        assert_eq!(from_file.mandate().unwrap(), mandate);
        assert!(
            from_file
                .vars()
                .contains(&(BotEnv::MANDATE, path.display().to_string()))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

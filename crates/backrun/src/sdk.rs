//! Native bot contract. Context contains only this searcher's signing key and
//! public setup; observations never include pending orders or rival bids.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use solana_message::AddressLookupTableAccount;
use solana_signer::Signer;

use crate::{fixture::Market, programs::key, wallet::Wallet};

#[derive(Clone, Serialize, Deserialize)]
pub struct Table {
    pub address: String,
    pub entries: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Context {
    pub version: u32,
    pub protocol: String,
    pub slot_ms: u64,
    pub participant: u8,
    pub market: Market,
    pub trader_keypair: Vec<u8>,
    pub executor: String,
    pub token_accounts: Vec<String>,
    pub tables: Vec<Table>,
}
impl Context {
    pub fn new(
        participant: u8,
        market: Market,
        session: &Wallet,
        tables: &[AddressLookupTableAccount],
    ) -> Self {
        Self {
            version: 3,
            protocol: crate::PROTOCOL.into(),
            slot_ms: crate::SLOT_MS,
            participant,
            market,
            trader_keypair: session.trader.to_bytes().to_vec(),
            executor: session.executor.to_string(),
            token_accounts: session
                .token_accounts
                .iter()
                .map(ToString::to_string)
                .collect(),
            tables: tables
                .iter()
                .map(|t| Table {
                    address: t.key.to_string(),
                    entries: t.addresses.iter().map(ToString::to_string).collect(),
                })
                .collect(),
        }
    }
    pub fn wallet(&self) -> Result<Wallet> {
        Ok(Wallet {
            executor: key(&self.executor),
            trader: solana_keypair::Keypair::try_from(self.trader_keypair.as_slice())?,
            token_accounts: self.token_accounts.iter().map(|s| key(s)).collect(),
        })
    }
    pub fn transaction(
        &self,
        instructions: Vec<solana_instruction::Instruction>,
        blockhash: &str,
        cu: u32,
        micro_lamports: u64,
    ) -> Result<Vec<u8>> {
        use solana_compute_budget_interface::ComputeBudgetInstruction as C;
        let mut all = vec![
            C::set_compute_unit_limit(cu),
            C::set_compute_unit_price(micro_lamports),
        ];
        all.extend(instructions);
        let session = self.wallet()?;
        let tables = self
            .tables
            .iter()
            .map(|t| AddressLookupTableAccount {
                key: key(&t.address),
                addresses: t.entries.iter().map(|s| key(s)).collect(),
            })
            .collect::<Vec<_>>();
        let message = solana_message::v0::Message::try_compile(
            &session.trader.pubkey(),
            &all,
            &tables,
            blockhash.parse()?,
        )?;
        let tx = solana_transaction::versioned::VersionedTransaction::try_new(
            solana_message::VersionedMessage::V0(message),
            &[&session.trader],
        )?;
        Ok(wincode::serialize(&tx)?)
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct PoolState {
    pub reserves: [u64; 2],
    /// Decimal strings preserve u128 precision in every bot language.
    pub sqrt_price: String,
    pub liquidity: String,
    pub tick: i32,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Observation {
    pub event: u64,
    pub revision: u64,
    pub open: bool,
    pub finished: bool,
    pub blockhash: String,
    pub pools: Vec<PoolState>,
}

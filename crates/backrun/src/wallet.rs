//! Ordinary signer-owned accounts; the optional route executor imposes no policy.
use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

use crate::{FEE_LAMPORTS, TRADING_LAMPORTS, WSOL, fixture::Market, programs::*, world::World};

pub struct Wallet {
    pub trader: Keypair,
    pub executor: Pubkey,
    pub token_accounts: Vec<Pubkey>,
}
impl Wallet {
    pub async fn prepare(world: &World, market: &Market, executor: Pubkey) -> Result<Self> {
        let trader = Keypair::new();
        world
            .surfnet
            .cheatcodes()
            .fund_sol(&trader.pubkey(), FEE_LAMPORTS)?;
        let mut token_accounts = vec![];
        for mint in &market.tokens {
            let mint = key(mint);
            world
                .fund_tokens(
                    trader.pubkey(),
                    mint,
                    if mint == key(WSOL) {
                        TRADING_LAMPORTS
                    } else {
                        0
                    },
                )
                .await?;
            token_accounts.push(ata(trader.pubkey(), mint));
        }
        Ok(Self {
            trader,
            executor,
            token_accounts,
        })
    }
    /// Optional convenience CPI router, called directly by the wallet signer.
    pub fn route(&self, swaps: &[Instruction], inputs: &[Pubkey]) -> Result<Instruction> {
        ensure!(
            !swaps.is_empty() && swaps.len() <= 8 && swaps.len() == inputs.len(),
            "invalid route"
        );
        let mut flags = BTreeMap::<Pubkey, (bool, bool)>::new();
        for ix in swaps {
            flags.entry(ix.program_id).or_default();
            for a in &ix.accounts {
                let f = flags.entry(a.pubkey).or_default();
                f.0 |= a.is_signer;
                f.1 |= a.is_writable;
            }
        }
        let keys = flags.keys().copied().collect::<Vec<_>>();
        ensure!(keys.len() < 256, "too many accounts");
        let index = |key: &Pubkey| -> Result<u8> {
            Ok(keys
                .iter()
                .position(|k| k == key)
                .ok_or_else(|| anyhow::anyhow!("missing input"))? as u8)
        };
        let mut data = vec![swaps.len() as u8];
        for (i, ix) in swaps.iter().enumerate() {
            data.extend([index(&ix.program_id)?, u8::from(i > 0), index(&inputs[i])?]);
            data.extend(u16::try_from(ix.data.len())?.to_le_bytes());
            data.push(u8::try_from(ix.accounts.len())?);
            for a in &ix.accounts {
                data.extend([
                    index(&a.pubkey)?,
                    u8::from(a.is_signer) | (u8::from(a.is_writable) << 1),
                ]);
            }
            data.extend(&ix.data);
        }
        let accounts = flags
            .into_iter()
            .map(|(pubkey, (is_signer, is_writable))| AccountMeta {
                pubkey,
                is_signer,
                is_writable,
            })
            .collect();
        Ok(Instruction {
            program_id: self.executor,
            accounts,
            data,
        })
    }
}

pub fn lookup_addresses(market: &Market, wallet: &Wallet) -> Vec<Pubkey> {
    let mut addresses = std::collections::BTreeSet::from([
        wallet.executor,
        key(crate::CPMM),
        key(crate::WHIRLPOOL),
        key(TOKEN),
        pda(key(crate::CPMM), &[b"vault_and_lp_mint_auth_seed"]),
    ]);
    addresses.extend(wallet.token_accounts.iter().copied());
    addresses.extend(market.tokens.iter().map(|s| key(s)));
    for p in &market.pools {
        addresses.extend([key(&p.address), key(&p.config), key(&p.observation)]);
        addresses.extend(p.vaults.iter().chain(&p.tick_arrays).map(|s| key(s)));
    }
    addresses.into_iter().collect()
}

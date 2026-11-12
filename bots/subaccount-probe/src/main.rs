//! A bot that trades from an account other than its cross-margin one, for
//! the subaccount check.
//!
//! On its first turn it registers an isolated Phoenix subaccount, syncs it
//! with its cross-margin account, moves collateral into it and buys twice
//! its position cap there: one transaction, which it signs and sends
//! itself. Then it holds. What the transaction did is logged as `probe:`
//! lines, which the check reads.

use mtx_kit::{
    BotEnv, Keypair, Phoenix, RpcClient, Signature, Signer,
    chain::{InstructionGroup, TransactionPlan},
    prelude::*,
    protocols::phoenix::eternal::phoenix_rise::{
        api::TraderKey,
        core::MarketOrderTicket,
        ix::market_order::create_place_market_order_ix,
        math::Side as PhoenixSide,
    },
    rpc_client,
};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use tracing::info;

/// The isolated subaccount it opens.
const SUBACCOUNT: u8 = 1;
/// The collateral it moves there, in USDC: less than the drawdown limit, so
/// a meter that misses the subaccount sees only a small loss.
const COLLATERAL_USDC: f64 = 600.0;

struct Probe {
    env: Option<BotEnv>,
    rpc: Option<RpcClient>,
    payer: Option<Keypair>,
    sent: Option<Signature>,
    reported: bool,
}

impl Probe {
    /// Register, sync, fund and buy, in one transaction.
    fn send(&self) -> Result<Signature, String> {
        let (env, rpc, payer) = (
            self.env.as_ref().unwrap(),
            self.rpc.as_ref().unwrap(),
            self.payer.as_ref().unwrap(),
        );
        let owner = payer.pubkey();
        let phoenix = Phoenix::new(env.exchange().map_err(|e| e.to_string())?, &env.rpc_url);
        let tx = phoenix.tx();
        let parent = phoenix.trader_account(&owner);
        let child = TraderKey::derive_pda(&owner, 0, SUBACCOUNT);
        let lots = phoenix.base_units_to_lots(
            phoenix.market(&env.symbol).map_err(|e| e.to_string())?,
            2.0 * env.max_position,
        );
        let ticket = MarketOrderTicket::builder()
            .authority(owner)
            .trader_account(child)
            .symbol(&env.symbol)
            .side(PhoenixSide::Bid)
            .num_base_lots(lots)
            .subaccount_index(SUBACCOUNT)
            .build()
            .map_err(|e| e.to_string())?;
        let params = ticket
            .to_params(
                tx.order_ticket_metadata(&env.symbol)
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        let mut instructions = vec![ComputeBudgetInstruction::set_compute_unit_price(5_000)];
        let built = [
            tx.build_register_trader(owner, 0, SUBACCOUNT),
            tx.build_sync_parent_to_child(owner, parent, child),
            tx.build_transfer_collateral(owner, parent, child, COLLATERAL_USDC),
        ];
        for ixs in built {
            instructions.extend(ixs.map_err(|e| e.to_string())?);
        }
        instructions.push(
            create_place_market_order_ix(params)
                .map_err(|e| e.to_string())?
                .into(),
        );
        let plan = TransactionPlan::from_group(
            &InstructionGroup::new("subaccount", instructions),
            1_400_000,
        );
        let blockhash = rpc.get_latest_blockhash().map_err(|e| e.to_string())?;
        let mut transaction = plan.transaction(&owner, blockhash);
        transaction.sign(&[payer], blockhash);
        rpc.send_transaction(&transaction)
            .map_err(|e| e.to_string())
    }
}

impl Strategy for Probe {
    fn start(&mut self, session: &Session) -> BotResult {
        // A process-only bot: it sends its own transactions.
        let _ = session;
        let env = BotEnv::from_env()?;
        self.rpc = Some(rpc_client(&env.rpc_url));
        self.payer = Some(env.keypair()?);
        self.env = Some(env);
        Ok(())
    }

    fn step(&mut self, market: &Market, _orders: &mut Orders) -> BotResult {
        let k = market.k;
        match self.sent {
            None => {
                let signature = self.send()?;
                info!(k, %signature, "probe: sent");
                self.sent = Some(signature);
            }
            Some(signature) if !self.reported => {
                let status = self
                    .rpc
                    .as_ref()
                    .unwrap()
                    .get_signature_statuses(&[signature])?
                    .value[0]
                    .clone();
                if let Some(status) = status {
                    match status.err {
                        None => info!(k, %signature, "probe: landed ok"),
                        Some(error) => info!(k, %signature, %error, "probe: failed"),
                    }
                    self.reported = true;
                }
            }
            Some(_) => {}
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Probe {
        env: None,
        rpc: None,
        payer: None,
        sent: None,
        reported: false,
    })
}

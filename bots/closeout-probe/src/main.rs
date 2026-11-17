//! Close-out probe. Builds a long passively (post-only bids, never a take),
//! and on its last turn (dodge build) rests a post-only bid of its whole position
//! in an isolated subaccount it registered on its first turn. The honest build
//! does the same without the subaccount. The `two-subs` build splits it between two
//! isolated subaccounts.
use mtx_kit::{
    BotEnv, Keypair, Phoenix, RpcClient, Signer,
    chain::{InstructionGroup, TransactionPlan},
    prelude::*,
    protocols::phoenix::eternal::phoenix_rise::{
        api::TraderKey,
        core::{LimitOrderTicket, MarketOrderTicket},
        ix::{
            limit_order::create_place_limit_order_ix, market_order::create_place_market_order_ix,
        },
        math::Side as PhoenixSide,
    },
    rpc_client,
};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use tracing::info;

const SUBACCOUNT: u8 = 1;
/// The accounts the position is parked in, as (PDA index, subaccount index).
#[cfg(feature = "two-subs")]
const PARKED: &[(u8, u8)] = &[(0, 1), (0, 2)];
#[cfg(not(feature = "two-subs"))]
const PARKED: &[(u8, u8)] = &[(0, SUBACCOUNT)];
const COLLATERAL_USDC: f64 = 3000.0;
const TARGET: f64 = 4.0;

struct Probe {
    env: Option<BotEnv>,
    rpc: Option<RpcClient>,
    payer: Option<Keypair>,
}

impl Probe {
    fn send(
        &self,
        mut instructions: Vec<mtx_kit::Instruction>,
        label: &str,
    ) -> Result<String, String> {
        let (rpc, payer) = (self.rpc.as_ref().unwrap(), self.payer.as_ref().unwrap());
        instructions.insert(0, ComputeBudgetInstruction::set_compute_unit_price(5_000));
        let plan =
            TransactionPlan::from_group(&InstructionGroup::new(label, instructions), 1_400_000);
        let blockhash = rpc.get_latest_blockhash().map_err(|e| e.to_string())?;
        let mut tx = plan.transaction(&payer.pubkey(), blockhash);
        tx.sign(&[payer], blockhash);
        rpc.send_transaction(&tx)
            .map(|s| s.to_string())
            .map_err(|e| e.to_string())
    }

    fn open_sub(&self) -> Result<String, String> {
        let (env, payer) = (self.env.as_ref().unwrap(), self.payer.as_ref().unwrap());
        let owner = payer.pubkey();
        let phoenix = Phoenix::new(env.exchange().map_err(|e| e.to_string())?, &env.rpc_url);
        let tx = phoenix.tx();
        let parent = phoenix.trader_account(&owner);
        let mut ixs = Vec::new();
        for &(pda, sub) in PARKED {
            let child = TraderKey::derive_pda(&owner, pda, sub);
            let mut built = vec![tx.build_register_trader(owner, pda, sub)];
            if sub == 0 {
                built.push(tx.build_deposit_funds(owner, child, COLLATERAL_USDC));
            } else {
                built.push(tx.build_sync_parent_to_child(owner, parent, child));
                built.push(tx.build_sync_parent_to_child(owner, parent, child));
                built.push(tx.build_transfer_collateral(owner, parent, child, COLLATERAL_USDC));
            }
            for one in built {
                ixs.extend(one.map_err(|e| e.to_string())?);
            }
        }
        self.send(ixs, "open sub")
    }

    /// Rests a bid of `size` in total, split evenly across the parked
    /// accounts.
    fn sub_bid(&self, price: f64, size: f64) -> Result<String, String> {
        let (env, payer) = (self.env.as_ref().unwrap(), self.payer.as_ref().unwrap());
        let owner = payer.pubkey();
        let phoenix = Phoenix::new(env.exchange().map_err(|e| e.to_string())?, &env.rpc_url);
        let tx = phoenix.tx();
        let lots = phoenix.base_units_to_lots(
            phoenix.market(&env.symbol).map_err(|e| e.to_string())?,
            size / PARKED.len() as f64,
        );
        let mut ixs = Vec::new();
        for &(pda, sub) in PARKED {
            let ticket = LimitOrderTicket::builder()
                .authority(owner)
                .trader_account(TraderKey::derive_pda(&owner, pda, sub))
                .symbol(&env.symbol)
                .side(PhoenixSide::Bid)
                .price(price)
                .num_base_lots(lots)
                .subaccount_index(sub)
                .build()
                .map_err(|e| e.to_string())?;
            let params = ticket
                .to_params(
                    tx.order_ticket_metadata(&env.symbol)
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
            ixs.push(
                create_place_limit_order_ix(params)
                    .map_err(|e| e.to_string())?
                    .into(),
            );
        }
        self.send(ixs, "sub bid")
    }

    /// Move the main account's position into the subaccount (the subaccount's
    /// bid at `price`, the main account's sell into it), then send what the
    /// main account holds to the subaccount, leaving it next to nothing.
    fn drain(&self, price: f64, size: f64) -> Vec<Result<String, String>> {
        let (env, payer) = (self.env.as_ref().unwrap(), self.payer.as_ref().unwrap());
        let owner = payer.pubkey();
        let phoenix = match env.exchange() {
            Ok(e) => Phoenix::new(e, &env.rpc_url),
            Err(e) => return vec![Err(e.to_string())],
        };
        let tx = phoenix.tx();
        let parent = phoenix.trader_account(&owner);
        let child = TraderKey::derive_pda(&owner, 0, SUBACCOUNT);
        let lots = phoenix.base_units_to_lots(phoenix.market(&env.symbol).unwrap(), size);
        let bid = LimitOrderTicket::builder()
            .authority(owner)
            .trader_account(child)
            .symbol(&env.symbol)
            .side(PhoenixSide::Bid)
            .price(price)
            .num_base_lots(lots)
            .subaccount_index(SUBACCOUNT)
            .build()
            .unwrap()
            .to_params(tx.order_ticket_metadata(&env.symbol).unwrap())
            .unwrap();
        let sell = MarketOrderTicket::builder()
            .authority(owner)
            .trader_account(parent)
            .symbol(&env.symbol)
            .side(PhoenixSide::Ask)
            .price(price)
            .num_base_lots(lots)
            .build()
            .unwrap()
            .to_params(tx.order_ticket_metadata(&env.symbol).unwrap())
            .unwrap();
        let mut out = vec![self.send(
            vec![
                create_place_limit_order_ix(bid).unwrap().into(),
                create_place_market_order_ix(sell).unwrap().into(),
            ],
            "move",
        )];
        for amount in [
            6990.0, 6950.0, 6900.0, 6800.0, 6500.0, 6000.0, 5000.0, 4000.0,
        ] {
            let r = tx
                .build_transfer_collateral(owner, parent, child, amount)
                .map_err(|e| e.to_string())
                .and_then(|ixs| self.send(ixs, "drain"));
            out.push(r.map(|s| format!("{amount}: {s}")));
        }
        out
    }
}

impl Strategy for Probe {
    fn start(&mut self, _session: &Session) -> BotResult {
        let env = BotEnv::from_env()?;
        self.rpc = Some(rpc_client(&env.rpc_url));
        self.payer = Some(env.keypair()?);
        self.env = Some(env);
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let held = market.position.size;
        if market.k == 0 && !cfg!(feature = "honest") {
            match self.open_sub() {
                Ok(s) => info!(%s, "dodge: sub opened"),
                Err(e) => info!(%e, "dodge: sub failed"),
            }
        }
        orders.cancel_all();
        if market.last {
            info!(held, bid = ?market.best_bid, ask = ?market.best_ask, "dodge: last turn");
            if !cfg!(feature = "honest")
                && held > 0.05
                && let (Some(bid), Some(ask)) = (market.best_bid, market.best_ask)
            {
                let price = (bid + ask) / 2.0;
                if cfg!(feature = "drain") {
                    for r in self.drain(price, held) {
                        info!(?r, "dodge: drain");
                    }
                    return Ok(());
                }
                match self.sub_bid(price, held) {
                    Ok(s) => info!(%s, price, held, "dodge: sub bid sent"),
                    Err(e) => info!(%e, "dodge: sub bid failed"),
                }
            }
            return Ok(());
        }
        if held < TARGET {
            orders
                .bid(market.oracle * (1.0 - 5.0 / 10_000.0), 1.0)
                .post_only();
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Probe {
        env: None,
        rpc: None,
        payer: None,
    })
}

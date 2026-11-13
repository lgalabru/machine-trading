//! The stake-pool session smoke bot as a strategy component. On the
//! session's first pool it stakes 5 SOL, asks to redeem 60 of the token at
//! once, more than the smoke's pool reserve pays, then redeems 2; on the
//! second pool, if there is one, it stakes 50 SOL and unstakes 1 of its
//! token at once. Every step it logs what became of its lines and each
//! pool's numbers.

use mtx_kit::{log, prelude::*};

#[derive(Default)]
pub struct LstSmoke {
    pools: Vec<String>,
}

impl Strategy for LstSmoke {
    fn start(&mut self, session: &Session) -> BotResult {
        self.pools = session.venues.iter().map(|v| v.id.clone()).collect();
        log(&format!(
            "start: {} steps, pools {:?}",
            session.steps, self.pools
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        for sent in &market.sends {
            log(&format!("sent {:?}: {:?}", sent.order, sent.state));
        }
        for view in &market.venues {
            log(&format!(
                "step {} {}: bid {:?} ask {:?} {:?}",
                market.k, view.id, view.best_bid, view.best_ask, view.stats
            ));
        }
        let second = self.pools.get(1).cloned();
        match (market.k, second) {
            (0, _) => {
                orders.deposit(5.0);
            }
            (1, _) => {
                orders.withdraw(Some(60.0));
            }
            (2, _) => {
                orders.withdraw(Some(2.0));
            }
            (3, Some(pool)) => {
                orders.deposit(50.0).at(&pool);
            }
            (4, Some(pool)) => {
                orders.withdraw(Some(1.0)).at(&pool);
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(LstSmoke);

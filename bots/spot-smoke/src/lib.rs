//! The spot session smoke bot as a strategy component: on its first step
//! it sells 1 SOL at any price on the venue its orders go to. Built with
//! feature `buy`, it also buys 0.1 SOL there, which a sell-only mandate
//! forbids; with `accumulate`, it buys 1 SOL instead of selling. Every step
//! it reads each market the session names through `rpc`, decodes its book
//! with the kit's venue decoders, and logs the market and the books.

use mtx_kit::{log, prelude::*};

/// A market the session names, as `start`'s `venue` lists it.
#[derive(Debug, Clone)]
struct Named {
    venue: String,
    market: String,
}

#[derive(Default)]
pub struct SpotSmoke {
    markets: Vec<Named>,
    sent: bool,
}

impl Strategy for SpotSmoke {
    fn start(&mut self, session: &Session) -> BotResult {
        let venues: serde_json::Value = serde_json::from_str(&session.venue)?;
        self.markets = venues["markets"]
            .as_array()
            .ok_or("no markets in the session's venue")?
            .iter()
            .map(|m| Named {
                venue: m["venue"].as_str().unwrap_or_default().to_string(),
                market: m["market"].as_str().unwrap_or_default().to_string(),
            })
            .collect();
        log(&format!(
            "start: {} steps, orders to {}, markets {:?}",
            session.steps, venues["orders"], self.markets
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let books: Vec<String> = self
            .markets
            .iter()
            .map(|m| match top(m) {
                Ok((bid, ask)) => format!("{} {bid:?}/{ask:?}", m.venue),
                Err(e) => format!("{} unread: {e}", m.venue),
            })
            .collect();
        log(&format!(
            "step {}: price {} bid {:?} ask {:?} position {} pnl {:.4} books [{}]",
            market.k,
            market.mark,
            market.best_bid,
            market.best_ask,
            market.position.size,
            market.pnl,
            books.join(", ")
        ));
        if self.sent {
            return Ok(());
        }
        self.sent = true;
        if cfg!(feature = "accumulate") {
            orders.buy(1.0, market.mark * 2.0);
        } else {
            orders.sell(1.0, 0.01);
            if cfg!(feature = "buy") {
                orders.buy(0.1, market.mark * 2.0);
            }
        }
        Ok(())
    }
}

/// A market's best bid and ask, read and decoded in the component.
#[cfg(target_arch = "wasm32")]
fn top(named: &Named) -> BotResult<(Option<String>, Option<String>)> {
    use mtx_kit::protocols::{
        openbook::v2::{MarketAccount, OpenBookV2Market, book_side_orders},
        phoenix::v1::{MarketState, PhoenixV1Market},
    };
    let address: mtx_kit::Pubkey = named.market.parse()?;
    let data = mtx_kit::rpc_account(&named.market)?.ok_or("no market account")?;
    let book = match named.venue.as_str() {
        "phoenix" => {
            let market = PhoenixV1Market::decode(address, &data, "SOL", "USDC")?;
            market.order_book(&MarketState::decode(&data)?, 0, 0)
        }
        _ => {
            let market = OpenBookV2Market::decode(address, &data, "SOL", "USDC")?;
            let account = MarketAccount::decode(&data)?;
            let sides =
                mtx_kit::rpc_accounts(&[&account.bids.to_string(), &account.asks.to_string()])?;
            let side = |i: usize| -> BotResult<Vec<_>> {
                let data = sides[i].as_ref().ok_or("no book side")?;
                Ok(book_side_orders(data)?)
            };
            market.order_book(&side(0)?, &side(1)?, 0, None)
        }
    };
    let price = |levels: &[mtx_kit::core::PriceLevel]| levels.first().map(|l| l.price.to_string());
    Ok((price(&book.bids), price(&book.asks)))
}

/// Natively there is no `rpc` to read the market with.
#[cfg(not(target_arch = "wasm32"))]
fn top(named: &Named) -> BotResult<(Option<String>, Option<String>)> {
    Err(format!("{} isn't read outside a component", named.market).into())
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(SpotSmoke);

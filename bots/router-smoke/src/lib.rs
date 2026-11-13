//! The router smoke, as a component: the pseudo-venue `router` and the
//! router's `rpc` methods. On the first step it asks `mtx_routerQuote` for
//! a sale of 300 of the base, then sells them through `router` at 5% under
//! the first pool's bid (a route big enough to split); on the second it
//! buys with 3,000 of the quote through `router`; on the third it sells 1
//! at twice the bid, which no route pays (the program refuses it); on the
//! fourth it asks `mtx_routerSwapInstructions` for a quote's transaction.
//! Every step it logs the router's notes and what became of its orders.

use mtx_kit::{AmountUnit, log, prelude::*, rpc};

#[derive(Default)]
struct RouterSmoke {
    base: String,
    quote: String,
    authority: String,
}

/// The value of `"name":"..."` in JSON text: enough for a smoke's log.
fn text_field<'a>(json: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("\"{name}\":\"");
    let at = json.find(&key)? + key.len();
    json[at..].split('"').next()
}

impl Strategy for RouterSmoke {
    fn start(&mut self, session: &Session) -> BotResult {
        let markets: Vec<_> = session.venues.iter().map(|v| v.id.as_str()).collect();
        log(&format!("start: markets {markets:?}"));
        // The session's pair, from its venue metadata's first market.
        self.base = text_field(&session.venue, "base_mint")
            .unwrap_or_default()
            .into();
        self.quote = text_field(&session.venue, "quote_mint")
            .unwrap_or_default()
            .into();
        self.authority = session.authority.clone();
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        for note in &market.notes {
            log(&format!("note: {note}"));
        }
        for sent in &market.sends {
            log(&format!(
                "sent {} {:?}: {:?}",
                sent.venue, sent.order, sent.state
            ));
        }
        let Some(first) = market.venues.first() else {
            return Ok(());
        };
        let (Some(bid), Some(ask)) = (first.best_bid, first.best_ask) else {
            return Ok(());
        };
        let quote = |amount: u64| {
            format!(
                r#"[{{"inputMint":"{}","outputMint":"{}","amount":"{amount}","slippageBps":100}}]"#,
                self.base, self.quote
            )
        };
        match market.k {
            0 => {
                match rpc("mtx_routerQuote", &quote(300_000_000)) {
                    Ok(answer) => log(&format!("quote: {answer}")),
                    Err(e) => log(&format!("quote refused: {e}")),
                }
                orders
                    .swap(Side::Ask, 300.0, AmountUnit::Base, bid * 0.95)
                    .at("router");
            }
            1 => {
                orders
                    .swap(Side::Bid, 3_000.0, AmountUnit::Quote, ask * 1.05)
                    .at("router");
            }
            2 => {
                orders
                    .swap(Side::Ask, 1.0, AmountUnit::Base, bid * 2.0)
                    .at("router");
            }
            3 => {
                let answer = rpc("mtx_routerQuote", &quote(5_000_000))?;
                let body = format!(
                    r#"[{{"quoteResponse":{answer},"userPublicKey":"{}"}}]"#,
                    self.authority
                );
                match rpc("mtx_routerSwapInstructions", &body) {
                    Ok(built) => log(&format!("swap instructions: {built}")),
                    Err(e) => log(&format!("swap instructions refused: {e}")),
                }
            }
            _ => {}
        }
        Ok(())
    }
}

mtx_kit::export!(RouterSmoke);

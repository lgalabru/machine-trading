//! A probe of the transaction layer (`tx-options` in `mtx:strategy@0.2.0`):
//! at chosen steps it sends a small order with one kind of option, and it
//! logs what became of every send as the session reports it: delayed,
//! then landed, failed or expired, with its slot, fee and tip.
//!
//! Its scenario (`scenario.json`) turns the layer on with a fee spike, a
//! congestion floor of 200,000 micro-lamports per compute unit over steps
//! 20 to 120, and fees counted in PnL. `just tx-probe` grades it in a
//! session; the grader's tests read its log.

use mtx_kit::{Route, SendSource, log, prelude::*};

#[derive(Default)]
struct Probe;

/// A price per compute unit well over the spike's floor.
const OVER_THE_FLOOR: u64 = 400_000;

fn options() -> TxOptions {
    TxOptions::default()
}

impl Strategy for Probe {
    fn start(&mut self, session: &Session) -> BotResult {
        log(&format!(
            "applies tx options {}, fee reports {}",
            session.applies_tx_options, session.reports.fees
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        for sent in &market.sends {
            let source = match sent.source {
                SendSource::Line(n) => format!("line {n}"),
                SendSource::Raw(n) => format!("raw {n}"),
            };
            let state = match &sent.state {
                SendState::Landed(_) => "landed".to_string(),
                SendState::Failed { reason, .. } => format!("failed ({reason})"),
                SendState::NotSent(why) => format!("not sent ({why})"),
                SendState::Delayed => "delayed".to_string(),
                SendState::Expired => "expired".to_string(),
            };
            log(&format!(
                "step {} {source}: {state}, slot {:?}, fee {:?}, tip {:?}",
                sent.step, sent.slot, sent.fee, sent.tip
            ));
        }
        if let Some(fees) = &market.fees
            && matches!(market.k, 0 | 25)
        {
            log(&format!(
                "fees at step {}: {:?}, tip floor {:?}, {} leaders",
                market.k,
                fees.cu_prices,
                fees.tip_floor,
                fees.leaders.len()
            ));
        }
        let (buy, sell) = (market.mark * 1.05, market.mark * 0.95);
        match market.k {
            // As sent: no price, the kit's limit.
            1 => {
                orders.buy(1.0, buy);
            }
            // A price, and a limit from the simulation.
            2 => {
                orders.sell(1.0, sell).tx(TxOptions {
                    cu_price: Some(50_000),
                    ..options()
                });
            }
            // Two lines as one bundle, tipped, not to be fronted.
            3 => {
                let bundle = TxOptions {
                    bundle: Some(7),
                    tip: Some(20_000),
                    route: Some(Route::Jito {
                        bundle_only: true,
                        dont_front: true,
                    }),
                    ..options()
                };
                orders.buy(1.0, buy).tx(bundle);
                orders.sell(1.0, sell).tx(bundle);
            }
            // A bundle whose second transaction fails on chain: neither lands.
            5 => {
                orders.buy(1.0, buy).tx(TxOptions {
                    bundle: Some(1),
                    tip: Some(10_000),
                    ..options()
                });
                orders.sell(1.0, sell).tx(TxOptions {
                    bundle: Some(1),
                    cu_limit: Some(2_000),
                    ..options()
                });
            }
            // One that lands and fails: too few compute units. It pays.
            6 => {
                orders.buy(1.0, buy).tx(TxOptions {
                    cu_limit: Some(2_000),
                    ..options()
                });
            }
            // In the spike: the cheap one waits and expires, the one over
            // the floor lands.
            22 => {
                orders.buy(1.0, buy);
                orders.sell(1.0, sell).tx(TxOptions {
                    cu_price: Some(OVER_THE_FLOOR),
                    cu_limit: Some(400_000),
                    ..options()
                });
            }
            // Cheap again, retried until well after the spike: signed again
            // once its blockhash expires, and landed once.
            24 => {
                orders.buy(1.0, buy).tx(TxOptions {
                    retry_until_slot: Some(market.slot + 300),
                    ..options()
                });
            }
            _ if market.last => {
                orders.target_position(0.0);
            }
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, _last: Option<&Market>) {}
}

mtx_kit::export!(Probe);

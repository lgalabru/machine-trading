//! A probe of the transaction layer (`tx-options` in `mtx:strategy@0.2.0`):
//! at chosen steps it sends a small order with one kind of option, and it
//! logs what became of every send as the session reports it: delayed,
//! then landed, failed or expired, with its slot, fee and tip.
//!
//! Its scenario (`scenario.json`) turns the layer on with a fee spike, a
//! congestion floor of 200,000 micro-lamports per compute unit over steps
//! 20 to 120, and fees counted in PnL, and lets raw sends run the system,
//! token and associated token programs. `just tx-probe` grades it in a
//! session; the grader's tests read its log.

use mtx_kit::{
    Route, SendSource, log,
    prelude::*,
    raw::{AccountKey, Derivation, RawAccount, RawInstruction, RawTransaction},
    send_raw,
};

const SYSTEM: &str = "11111111111111111111111111111111";
const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

#[derive(Default)]
struct Probe;

/// A price per compute unit well over the spike's floor.
const OVER_THE_FLOOR: u64 = 400_000;

fn options() -> TxOptions {
    TxOptions::default()
}

/// A raw send that changes nothing it is judged on: the associated token
/// program's `CreateIdempotent` for the bot's own USDC account.
fn own_usdc_account(tx: TxOptions) -> RawTransaction {
    let account = |key, signer, writable| RawAccount {
        key,
        signer,
        writable,
    };
    let address = |a: &str| AccountKey::Address(a.into());
    RawTransaction {
        instructions: vec![RawInstruction {
            program: ASSOCIATED_TOKEN.into(),
            accounts: vec![
                account(AccountKey::Authority, true, true),
                account(AccountKey::Derived(0), false, true),
                account(AccountKey::Authority, false, false),
                account(address(USDC), false, false),
                account(address(SYSTEM), false, false),
                account(address(TOKEN), false, false),
            ],
            data: vec![1],
        }],
        derived: vec![Derivation::Ata {
            owner: AccountKey::Authority,
            mint: address(USDC),
            token_program: address(TOKEN),
        }],
        tx: Some(tx),
        ..RawTransaction::default()
    }
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
            // A line and a raw send as one bundle: it goes in the step they
            // were sent, and they land together.
            8 => {
                let bundle = TxOptions {
                    bundle: Some(3),
                    ..options()
                };
                orders.buy(1.0, buy).tx(TxOptions {
                    tip: Some(40_000),
                    ..bundle
                });
                log(&format!(
                    "raw at step 8: {:?}",
                    send_raw(&own_usdc_account(bundle))
                ));
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
            // The last step's bundled raw send still goes.
            _ if market.last => {
                orders.target_position(0.0);
                log(&format!(
                    "raw at the last step: {:?}",
                    send_raw(&own_usdc_account(TxOptions {
                        bundle: Some(5),
                        tip: Some(10_000),
                        ..options()
                    }))
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, _last: Option<&Market>) {}
}

mtx_kit::export!(Probe);

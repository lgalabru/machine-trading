//! A [`Strategy`](crate::Strategy) as a WebAssembly component the grader
//! calls (feature `component`; `docs/rfd/0002-wasm-strategies.md`). The world
//! is `wit/strategy.wit`, `mtx:strategy@0.2.0`; the grader still loads a
//! component built on 0.1.0 (`wit/0.1.0/strategy.wit`).
//!
//! Build the strategy's crate as a `cdylib` for `wasm32-wasip2`, with
//! `mtx-kit` at `default-features = false, features = ["component"]`, and
//! export the strategy with [`export!`](crate::export). The output is a
//! component; name it `bot.wasm` in the bot's directory.

#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub mod bindings {
    wit_bindgen::generate!({
        world: "strategy",
        path: "wit/strategy.wit",
        pub_export_macro: true,
        default_bindings_module: "mtx_kit::component::bindings",
    });
}

use bindings::mtx::strategy::types as wit;
use mtx_core::turns;

use crate::{
    Action, AmountUnit, Book, Execution, FeeView, Intent, LendingPosition, LineOptions,
    LiquidityPosition, LiquidityShape, Market, MarketKind, OpenOrder, OwnFill, Position,
    PriceRange, Reports, Rfq, Role, Route, SendSource, SendState, SendStatus, Session, Side,
    TimeInForce, VenueInfo, VenueView,
};

/// The session, as the grader hands it to `start`.
#[doc(hidden)]
pub fn session(s: wit::Session) -> Result<Session, String> {
    Ok(Session {
        steps: s.steps,
        slots_per_step: s.slots_per_step,
        start_slot: s.start_slot,
        end_slot: s.end_slot,
        symbol: s.symbol,
        start_mark: s.start_mark,
        start_oracle: s.start_oracle,
        cap: s.cap,
        max_drawdown: s.max_drawdown,
        mandate: serde_json::from_str(&s.mandate).map_err(|e| format!("the mandate: {e}"))?,
        venue: s.venue,
        authority: s.authority,
        vars: s.vars,
        seed: s.seed,
        venues: s
            .venues
            .into_iter()
            .map(|v| VenueInfo {
                id: v.id,
                kind: match v.kind {
                    wit::MarketKind::Perp => MarketKind::Perp,
                    wit::MarketKind::Spot => MarketKind::Spot,
                    wit::MarketKind::Amm => MarketKind::Amm,
                    wit::MarketKind::Lending => MarketKind::Lending,
                    wit::MarketKind::StakePool => MarketKind::StakePool,
                },
                address: v.address,
                tradable: v.tradable,
            })
            .collect(),
        reports: Reports {
            tape: s.reports.tape,
            depth: s.reports.depth,
            timestamps: s.reports.timestamps,
            fees: s.reports.fees,
        },
        applies_tx_options: s.applies_tx_options,
    })
}

/// The market, as the grader hands it to `step`. The venues' tapes and
/// books are also [`Market::tape`] and [`Market::depth`], as a process gets
/// them.
#[doc(hidden)]
pub fn market(m: wit::Market) -> Market {
    let venues: Vec<VenueView> = m.venues.into_iter().map(venue_view).collect();
    let name = |id: &str| id.split(':').next().unwrap_or_default().to_string();
    Market {
        k: m.k,
        slot: m.slot,
        last: m.last,
        steps_left: m.steps_left,
        slots_left: m.slots_left,
        mark: m.mark,
        oracle: m.oracle,
        index: m.index,
        best_bid: m.best_bid,
        best_ask: m.best_ask,
        position: position(m.position),
        cap: m.cap,
        max_drawdown: m.max_drawdown,
        resting_bids: m.resting_bids,
        resting_asks: m.resting_asks,
        pnl: m.pnl,
        peak_pnl: m.peak_pnl,
        drawdown: m.drawdown,
        tape: venues
            .iter()
            .flat_map(|v| v.tape.iter().flatten().cloned())
            .collect(),
        depth: venues
            .iter()
            .filter_map(|v| {
                let book = v.depth.as_ref()?;
                Some(turns::BookLevels {
                    venue: name(&v.id),
                    market: Some(v.id.clone()),
                    bids: book.bids.clone(),
                    asks: book.asks.clone(),
                })
            })
            .collect(),
        unix_timestamp: m.unix_timestamp,
        venues,
        sends: m.sends.into_iter().map(send_status).collect(),
        fills: m
            .fills
            .into_iter()
            .map(|f| OwnFill {
                venue: f.venue,
                slot: f.slot,
                side: kit_side(f.side),
                price: f.price,
                size: f.size,
                role: role(f.role),
            })
            .collect(),
        rfqs: m
            .rfqs
            .into_iter()
            .map(|r| Rfq {
                id: r.id,
                venue: r.venue,
                side: r.side.map(kit_side),
                size: r.size,
                deadline_slot: r.deadline_slot,
            })
            .collect(),
        notes: m.notes,
        fees: m.fees.map(|f| FeeView {
            cu_prices: f.cu_prices,
            tip_floor: f.tip_floor,
            leaders: f.leaders,
        }),
    }
}

fn position(p: wit::Position) -> Position {
    Position {
        size: p.size,
        entry_usd: p.entry_usd,
    }
}

fn role(role: wit::Role) -> Role {
    match role {
        wit::Role::Taker => Role::Taker,
        wit::Role::Maker => Role::Maker,
    }
}

fn range(r: wit::PriceRange) -> PriceRange {
    PriceRange {
        lower: r.lower,
        upper: r.upper,
    }
}

fn venue_view(v: wit::VenueView) -> VenueView {
    let name = v.id.split(':').next().unwrap_or_default().to_string();
    let levels = |levels: Vec<wit::Level>| {
        levels
            .into_iter()
            .map(|l| turns::Level {
                price: l.price,
                base: l.size,
            })
            .collect()
    };
    VenueView {
        best_bid: v.best_bid,
        best_ask: v.best_ask,
        depth: v.depth.map(|book| Book {
            bids: levels(book.bids),
            asks: levels(book.asks),
        }),
        tape: v.tape.map(|tape| {
            tape.into_iter()
                .map(|t| turns::Trade {
                    venue: name.clone(),
                    market: Some(v.id.clone()),
                    side: match t.side {
                        wit::Side::Bid => mtx_core::Side::Buy,
                        wit::Side::Ask => mtx_core::Side::Sell,
                    },
                    price: t.price,
                    base: t.size,
                    slot: t.slot,
                    taker: t.taker,
                    maker: t.maker,
                    own: t.role.map(role),
                })
                .collect()
        }),
        position: v.position.map(position),
        orders: v
            .orders
            .into_iter()
            .map(|o| OpenOrder {
                client_id: o.client_id,
                side: kit_side(o.side),
                price: o.price,
                size: o.size,
            })
            .collect(),
        liquidity: v
            .liquidity
            .into_iter()
            .map(|p| LiquidityPosition {
                id: p.id,
                range: p.range.map(range),
                base: p.base,
                quote: p.quote,
                fees_base: p.fees_base,
                fees_quote: p.fees_quote,
            })
            .collect(),
        lending: v.lending.map(|l| LendingPosition {
            supplied: l.supplied,
            borrowed: l.borrowed,
            health: l.health,
        }),
        stats: v.stats,
        id: v.id,
    }
}

fn send_status(s: wit::SendStatus) -> SendStatus {
    let expires_slot = match &s.order {
        Some(wit::Order::Limit(o)) => o.expires_slot,
        Some(wit::Order::Respond(r)) => r.expiry_slot,
        _ => None,
    };
    SendStatus {
        expires_slot,
        step: s.step,
        source: match s.source {
            wit::SendSource::Line(n) => SendSource::Line(n),
            wit::SendSource::Raw(n) => SendSource::Raw(n),
        },
        venue: s.venue,
        client_id: s.client_id,
        order: s.order.map(|order| match order {
            wit::Order::Market(o) => Action::Market {
                side: kit_side(o.side),
                size: o.size,
                limit: o.limit,
            },
            wit::Order::Limit(o) => match o.time_in_force {
                wit::TimeInForce::Gtc => Action::Limit {
                    side: kit_side(o.side),
                    price: o.price,
                    size: o.size,
                },
                wit::TimeInForce::PostOnly => Action::PostOnly {
                    side: kit_side(o.side),
                    price: o.price,
                    size: o.size,
                },
                wit::TimeInForce::Ioc => Action::Market {
                    side: kit_side(o.side),
                    size: o.size,
                    limit: o.price,
                },
            },
            wit::Order::CancelAll => Action::CancelAll,
            wit::Order::Cancel(client_id) => Action::Cancel { client_id },
            wit::Order::Respond(r) => Action::Respond {
                id: r.id,
                bid: r.bid,
                ask: r.ask,
                size: r.size,
            },
            wit::Order::Swap(o) => Action::Swap {
                side: kit_side(o.side),
                amount: o.amount,
                unit: kit_unit(o.unit),
                limit: o.limit,
            },
            wit::Order::AddLiquidity(o) => Action::AddLiquidity {
                base: o.base,
                quote: o.quote,
                range: o.range.map(range),
                position: o.position,
                shape: o.shape.map(kit_shape),
            },
            wit::Order::RemoveLiquidity(o) => Action::RemoveLiquidity {
                position: o.position,
                share: o.share,
            },
            wit::Order::Deposit(a) => Action::Deposit { amount: a.amount },
            wit::Order::Withdraw(a) => Action::Withdraw { amount: a.amount },
            wit::Order::Borrow(a) => Action::Borrow { amount: a.amount },
            wit::Order::Repay(a) => Action::Repay { amount: a.amount },
            wit::Order::Liquidate(o) => Action::Liquidate {
                obligation: o.obligation,
                collateral: o.collateral,
                repay: o.repay,
                min_received: o.min_received,
            },
        }),
        state: match s.state {
            wit::SendState::Landed(signature) => SendState::Landed(signature),
            wit::SendState::Failed(failure) => SendState::Failed {
                reason: failure.reason,
                signature: failure.signature,
            },
            wit::SendState::NotSent(why) => SendState::NotSent(why),
            wit::SendState::Delayed => SendState::Delayed,
            wit::SendState::Expired => SendState::Expired,
        },
        slot: s.slot,
        fee: s.fee,
        tip: s.tip,
    }
}

fn kit_side(side: wit::Side) -> Side {
    match side {
        wit::Side::Bid => Side::Bid,
        wit::Side::Ask => Side::Ask,
    }
}

fn side(side: Side) -> wit::Side {
    match side {
        Side::Bid => wit::Side::Bid,
        Side::Ask => wit::Side::Ask,
    }
}

fn kit_shape(shape: wit::LiquidityShape) -> LiquidityShape {
    match shape {
        wit::LiquidityShape::Spot => LiquidityShape::Spot,
        wit::LiquidityShape::Curve => LiquidityShape::Curve,
        wit::LiquidityShape::BidAsk => LiquidityShape::BidAsk,
    }
}

fn kit_unit(unit: wit::AmountUnit) -> AmountUnit {
    match unit {
        wit::AmountUnit::Base => AmountUnit::Base,
        wit::AmountUnit::Quote => AmountUnit::Quote,
    }
}

fn wit_range(r: PriceRange) -> wit::PriceRange {
    wit::PriceRange {
        lower: r.lower,
        upper: r.upper,
    }
}

/// A line of the sheet, as the grader takes it from `step`.
#[doc(hidden)]
pub fn intent((intent, options): (Intent, LineOptions)) -> wit::Intent {
    let time_in_force = match options.time_in_force {
        TimeInForce::Gtc => wit::TimeInForce::Gtc,
        TimeInForce::PostOnly => wit::TimeInForce::PostOnly,
        TimeInForce::Ioc => wit::TimeInForce::Ioc,
    };
    let amount = |amount| wit::Amount { amount };
    let action = match intent {
        Intent::Target {
            position,
            execution:
                Execution {
                    max_per_step,
                    slice,
                    slippage_bps,
                    min_trade,
                },
        } => wit::Action::Target(wit::Target {
            position,
            execution: wit::Execution {
                max_per_step,
                slice,
                slippage_bps,
                min_trade,
                participation: options.participation,
            },
        }),
        Intent::Quote { bid, ask, size } => wit::Action::Quote(wit::Quote {
            bid,
            ask,
            size,
            post_only: options.time_in_force == TimeInForce::PostOnly,
            expires_slot: options.expires_slot,
        }),
        Intent::Market {
            side: s,
            size,
            limit,
        } => wit::Action::Market(wit::MarketOrder {
            side: side(s),
            size,
            limit,
        }),
        Intent::Limit {
            side: s,
            price,
            size,
        } => wit::Action::Limit(wit::LimitOrder {
            side: side(s),
            price,
            size,
            time_in_force,
            expires_slot: options.expires_slot,
        }),
        Intent::CancelAll => wit::Action::CancelAll,
        Intent::Cancel { client_id } => wit::Action::Cancel(client_id),
        Intent::Respond { id, bid, ask, size } => wit::Action::Respond(wit::RfqResponse {
            id,
            bid,
            ask,
            size,
            expiry_slot: options.expires_slot,
        }),
        Intent::Swap {
            side: s,
            amount,
            unit,
            limit,
        } => wit::Action::Swap(wit::Swap {
            side: side(s),
            amount,
            unit: match unit {
                AmountUnit::Base => wit::AmountUnit::Base,
                AmountUnit::Quote => wit::AmountUnit::Quote,
            },
            limit,
        }),
        Intent::AddLiquidity {
            base,
            quote,
            range,
            position,
            shape,
        } => wit::Action::AddLiquidity(wit::AddLiquidity {
            base,
            quote,
            range: range.map(wit_range),
            position,
            shape: shape.map(|shape| match shape {
                LiquidityShape::Spot => wit::LiquidityShape::Spot,
                LiquidityShape::Curve => wit::LiquidityShape::Curve,
                LiquidityShape::BidAsk => wit::LiquidityShape::BidAsk,
            }),
        }),
        Intent::RemoveLiquidity { position, share } => {
            wit::Action::RemoveLiquidity(wit::RemoveLiquidity { position, share })
        }
        Intent::Deposit { amount: a } => wit::Action::Deposit(amount(a)),
        Intent::Withdraw { amount: a } => wit::Action::Withdraw(amount(a)),
        Intent::Borrow { amount: a } => wit::Action::Borrow(amount(a)),
        Intent::Repay { amount: a } => wit::Action::Repay(amount(a)),
        Intent::Liquidate {
            obligation,
            collateral,
            repay,
            min_received,
        } => wit::Action::Liquidate(wit::Liquidate {
            obligation,
            collateral,
            repay,
            min_received,
        }),
    };
    wit::Intent {
        action,
        venue: options.venue,
        client_id: options.client_id,
        reduce_only: options.reduce_only,
        tx: options.tx.map(|tx| wit::TxOptions {
            cu_limit: tx.cu_limit,
            cu_price: tx.cu_price,
            tip: tx.tip,
            bundle: tx.bundle,
            route: tx.route.map(|route| match route {
                Route::Rpc => wit::Route::Rpc,
                Route::Staked => wit::Route::Staked,
                Route::Jito {
                    bundle_only,
                    dont_front,
                } => wit::Route::Jito(wit::JitoRoute {
                    bundle_only,
                    dont_front,
                }),
            }),
            retry_until_slot: tx.retry_until_slot,
        }),
    }
}

pub use crate::log;

/// Exports a [`Strategy`](crate::Strategy) that is `Default` as the
/// component's `start`, `step` and `end`.
#[macro_export]
macro_rules! export {
    ($strategy:ty) => {
        struct MtxStrategyComponent;

        thread_local! {
            static MTX_STRATEGY: ::std::cell::RefCell<$strategy> =
                ::std::cell::RefCell::new(<$strategy as ::std::default::Default>::default());
        }

        impl $crate::component::bindings::Guest for MtxStrategyComponent {
            fn start(
                session: $crate::component::bindings::mtx::strategy::types::Session,
            ) -> ::std::result::Result<(), ::std::string::String> {
                let session = $crate::component::session(session)?;
                MTX_STRATEGY.with(|s| {
                    $crate::Strategy::start(&mut *s.borrow_mut(), &session).map_err(|e| e.to_string())
                })
            }

            fn step(
                market: $crate::component::bindings::mtx::strategy::types::Market,
            ) -> ::std::result::Result<
                ::std::vec::Vec<$crate::component::bindings::mtx::strategy::types::Intent>,
                ::std::string::String,
            > {
                let market = $crate::component::market(market);
                let mut orders = $crate::Orders::new();
                MTX_STRATEGY.with(|s| {
                    $crate::Strategy::step(&mut *s.borrow_mut(), &market, &mut orders)
                        .map_err(|e| e.to_string())
                })?;
                Ok(orders.into_lines().into_iter().map($crate::component::intent).collect())
            }

            fn end(last: ::std::option::Option<$crate::component::bindings::mtx::strategy::types::Market>) {
                let last = last.map($crate::component::market);
                MTX_STRATEGY.with(|s| $crate::Strategy::end(&mut *s.borrow_mut(), last.as_ref()))
            }
        }

        $crate::component::bindings::export!(MtxStrategyComponent with_types_in $crate::component::bindings);
    };
}

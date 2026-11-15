//! Scoring v2's informed bot (the 1): the reference's desk, told the
//! session's latent state (the volatility state, each state's level, and
//! the flush once it has happened) through the ladder's `tell_regime`, and
//! nothing of the session's future. It trades the same market with the
//! same costs, mandate and turns. See `LevCarry::informed`.

fn main() -> mtx_kit::BotResult {
    mtx_kit::run(mtx_bot_levcarry::LevCarry::informed())
}

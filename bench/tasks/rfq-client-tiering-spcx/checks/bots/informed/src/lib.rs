//! The informed bot, scoring v2's 1: the reference's desk, told who the
//! informed clients are and their process, and nothing of the session's
//! draws.
//!
//! It reads `MTX_REGIME`, what the verifier's `--informed` hands it: the
//! informed clients' ids (`wallets.rfq-informed`, resolved by the session
//! from the latent's roster), how far ahead they see (`lead_slots`) and the
//! reach (`tolerance_bps`). It isn't told the path, when anyone asks, or
//! which way.
//!
//! - It declines every informed client's request, from the first.
//! - An informed request says which way SPCX is going over the lead: until
//!   the lead has passed, it doesn't sell to a buyer (or buy from a seller)
//!   on that side.
//! - It quotes every other request just inside its reach.

use mtx_bot_rfq_client_tiering::{Answer, Ask, EPS_BP, Policy, Tag, Told, View, reach_bp};
use mtx_kit::prelude::*;

#[derive(Default)]
pub struct Informed {
    told: Told,
}

impl Policy for Informed {
    fn start(&mut self, session: &Session) {
        self.told = Told::read(session);
    }

    fn answer(&mut self, view: &View<'_>, ask: &Ask) -> Answer {
        if self.told.informed.contains(&ask.client) {
            return Answer::Decline;
        }
        let lead = self.told.lead_slots;
        let last = view
            .requests
            .iter()
            .rev()
            .skip(1)
            .take_while(|r| r.slot + lead > view.slot)
            .find(|r| self.told.informed.contains(&r.client));
        if last.is_some_and(|r| r.sign == ask.sign) {
            return Answer::Decline;
        }
        Answer::Quote {
            bp: reach_bp(self.told.tolerance_bps, ask.n) - EPS_BP,
            size: None,
            tag: Tag::Other,
        }
    }
}

/// The informed bot: the desk with the told policy.
pub type Bot = mtx_bot_rfq_client_tiering::Desk<Informed>;

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Bot);

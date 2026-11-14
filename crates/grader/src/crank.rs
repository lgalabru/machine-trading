//! Venue cranks: upkeep a venue needs as time passes.
//!
//! Some venues only stay usable if someone keeps them current. Kamino's
//! reserves need a refresh in the same slot as a borrow, and lose their price
//! age checks across an epoch; an SPL stake pool refuses deposits until its
//! validator balances are updated. On mainnet a keeper does it. In a session
//! the engine owns the clock, so it owns the keeper: every step, after the
//! step's blocks land and the bot's held sends are released, each
//! [`VenueCrank`] gets a turn (see [`Cranks::step`]).
//!
//! A crank acts the way a keeper would, with ordinary transactions from an
//! account the grader holds: the cheatcodes are locked by then.
//!
//! # Failures
//!
//! A crank can fail in two ways, and they are told apart:
//!
//! - **The venue refused the transaction** ([`CrankOutcome::Refused`]). The
//!   bot shares the venue and can cause this (a pool it drained, a market it
//!   crossed). The session goes on, the refusal is recorded in the report's
//!   `crank_refusals`, and the crank is called again next step.
//! - **The grader or its RPC failed** (`Err`). Nothing the bot did explains
//!   it. The session returns it as [`crate::Error::Setup`], so the grade runs
//!   the session again from a fresh surfnet when the error is transient
//!   (see `grade.rs`) and otherwise fails.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solana_rpc_client::rpc_client::RpcClient;

use crate::{Result, keeper::LiquidatorReport, risk::ForcedClose};

/// Where in the session a crank is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrankStep {
    /// The step, from 0.
    pub k: u64,
    /// The slot the step's blocks reached.
    pub slot: u64,
    /// Slots since the session started.
    pub at: u64,
    pub last: bool,
}

/// What a crank's turn came to, when the grader itself was fine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrankOutcome {
    Done,
    /// The venue refused the crank's transaction on chain: the reason.
    Refused(String),
}

/// A turn that came to [`CrankOutcome::Refused`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CrankRefusal {
    pub crank: String,
    pub step: u64,
    pub reason: String,
}

/// What a session's cranks did, once it is over.
#[derive(Debug, Default)]
pub struct CrankReport {
    pub refusals: Vec<CrankRefusal>,
    pub closes: Vec<ForcedClose>,
    pub liquidator: Option<LiquidatorReport>,
}

/// Upkeep for one venue.
#[async_trait]
pub trait VenueCrank: Send {
    fn name(&self) -> &str;

    /// Called once a step, after its blocks have landed. `Err` is for the
    /// grader's own failures only; see the module docs.
    async fn crank(&mut self, rpc: &RpcClient, step: &CrankStep) -> Result<CrankOutcome>;

    /// Called once after the session's last step, with the bot stopped and
    /// before the account is measured a last time: a crank that acts on the
    /// account (a keeper) gets a final turn. Upkeep has none.
    async fn settle(&mut self, _rpc: &RpcClient, _step: &CrankStep) -> Result<CrankOutcome> {
        Ok(CrankOutcome::Done)
    }

    /// Closes the crank made on the bot's account, for the session's report
    /// (see [`crate::risk`]). Upkeep makes none.
    fn forced_closes(&mut self) -> Vec<ForcedClose> {
        Vec::new()
    }

    /// What a keeper did, for the session's report.
    fn liquidator_report(&self) -> Option<LiquidatorReport> {
        None
    }
}

/// Makes a session's cranks. A session builds its own, so a rerun starts
/// from fresh state.
pub type CrankFactory = dyn Fn() -> Vec<Box<dyn VenueCrank>> + Send + Sync;

/// A session's cranks, in the order they run.
#[derive(Default)]
pub struct Cranks {
    cranks: Vec<Box<dyn VenueCrank>>,
    refusals: Vec<CrankRefusal>,
}

impl Cranks {
    pub fn new(factory: Option<&CrankFactory>) -> Self {
        Self {
            cranks: factory.map(|make| make()).unwrap_or_default(),
            refusals: Vec::new(),
        }
    }

    /// A crank the session's kind brings, after the factory's.
    pub fn push(&mut self, crank: Box<dyn VenueCrank>) {
        self.cranks.push(crank);
    }

    pub fn is_empty(&self) -> bool {
        self.cranks.is_empty()
    }

    /// Every crank takes its turn, even after another was refused. A grader
    /// failure stops the step, wrapped as [`crate::Error::Setup`].
    pub async fn step(&mut self, rpc: &RpcClient, step: &CrankStep) -> Result<()> {
        for crank in &mut self.cranks {
            match crank
                .crank(rpc, step)
                .await
                .map_err(|e| crate::Error::Setup(Box::new(e)))?
            {
                CrankOutcome::Done => {}
                CrankOutcome::Refused(reason) => self.refusals.push(CrankRefusal {
                    crank: crank.name().to_string(),
                    step: step.k,
                    reason,
                }),
            }
        }
        Ok(())
    }

    /// Every crank's final turn, after the session's last step (see
    /// [`VenueCrank::settle`]). Refusals are recorded like any other.
    pub async fn settle(&mut self, rpc: &RpcClient, step: &CrankStep) -> Result<()> {
        for crank in &mut self.cranks {
            match crank
                .settle(rpc, step)
                .await
                .map_err(|e| crate::Error::Setup(Box::new(e)))?
            {
                CrankOutcome::Done => {}
                CrankOutcome::Refused(reason) => self.refusals.push(CrankRefusal {
                    crank: crank.name().to_string(),
                    step: step.k,
                    reason,
                }),
            }
        }
        Ok(())
    }

    /// What the cranks did, for the session's report: their refusals, the
    /// closes they made on the bot's account, and a keeper's account of
    /// itself.
    pub fn into_report(mut self) -> CrankReport {
        let closes = self
            .cranks
            .iter_mut()
            .flat_map(|crank| crank.forced_closes())
            .collect();
        let liquidator = self.cranks.iter().find_map(|crank| crank.liquidator_report());
        CrankReport {
            refusals: self.refusals,
            closes,
            liquidator,
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A crank that records the steps it was called at, and is refused or
    /// breaks at one if asked to.
    pub(crate) struct Recorder {
        pub name: &'static str,
        pub calls: Arc<Mutex<Vec<(&'static str, CrankStep)>>>,
        pub refuse_at: Option<u64>,
        pub fail_at: Option<u64>,
    }

    #[async_trait]
    impl VenueCrank for Recorder {
        fn name(&self) -> &str {
            self.name
        }

        async fn crank(&mut self, _rpc: &RpcClient, step: &CrankStep) -> Result<CrankOutcome> {
            self.calls.lock().unwrap().push((self.name, *step));
            if self.fail_at == Some(step.k) {
                return Err(crate::Error::Timeout(format!("{} crank", self.name)));
            }
            if self.refuse_at == Some(step.k) {
                return Ok(CrankOutcome::Refused("custom program error: 0x11".into()));
            }
            Ok(CrankOutcome::Done)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{testing::Recorder, *};

    type Calls = Arc<Mutex<Vec<(&'static str, CrankStep)>>>;

    fn step(k: u64) -> CrankStep {
        CrankStep {
            k,
            slot: 100 + 2 * k,
            at: 2 * k,
            last: k == 2,
        }
    }

    fn rpc() -> RpcClient {
        RpcClient::new("http://127.0.0.1:1".to_string())
    }

    fn recorder(
        name: &'static str,
        calls: &Calls,
        refuse_at: Option<u64>,
        fail_at: Option<u64>,
    ) -> Box<dyn VenueCrank> {
        Box::new(Recorder {
            name,
            calls: calls.clone(),
            refuse_at,
            fail_at,
        })
    }

    fn names(calls: &Calls) -> Vec<&'static str> {
        calls.lock().unwrap().iter().map(|c| c.0).collect()
    }

    #[tokio::test]
    async fn cranks_run_in_order_each_step() {
        let calls = Calls::default();
        let make = {
            let calls = calls.clone();
            move || {
                vec![
                    recorder("a", &calls, None, None),
                    recorder("b", &calls, None, None),
                ]
            }
        };
        let mut cranks = Cranks::new(Some(&make));
        for k in 0..3 {
            cranks.step(&rpc(), &step(k)).await.unwrap();
        }
        assert_eq!(names(&calls), ["a", "b", "a", "b", "a", "b"]);
        assert_eq!(calls.lock().unwrap()[5].1, step(2));
        assert!(cranks.into_report().refusals.is_empty());
    }

    #[tokio::test]
    async fn a_refused_crank_is_recorded_and_the_session_goes_on() {
        let calls = Calls::default();
        let make = {
            let calls = calls.clone();
            move || {
                vec![
                    recorder("a", &calls, Some(1), None),
                    recorder("b", &calls, None, None),
                ]
            }
        };
        let mut cranks = Cranks::new(Some(&make));
        for k in 0..3 {
            cranks.step(&rpc(), &step(k)).await.unwrap();
        }
        assert_eq!(
            names(&calls).len(),
            6,
            "b ran after a was refused, and a ran again"
        );
        let refusals = cranks.into_report().refusals;
        assert_eq!(refusals.len(), 1);
        assert_eq!((refusals[0].crank.as_str(), refusals[0].step), ("a", 1));
        assert!(refusals[0].reason.contains("0x11"));
    }

    #[tokio::test]
    async fn a_grader_failure_stops_the_step_as_a_setup_error() {
        let calls = Calls::default();
        let make = {
            let calls = calls.clone();
            move || {
                vec![
                    recorder("a", &calls, None, Some(1)),
                    recorder("b", &calls, None, None),
                ]
            }
        };
        let mut cranks = Cranks::new(Some(&make));
        cranks.step(&rpc(), &step(0)).await.unwrap();
        let error = cranks.step(&rpc(), &step(1)).await.unwrap_err();
        assert!(
            matches!(error, crate::Error::Setup(inner) if matches!(*inner, crate::Error::Timeout(_)))
        );
        assert_eq!(names(&calls), ["a", "b", "a"], "b never ran at step 1");
    }

    #[test]
    fn no_factory_means_no_cranks() {
        assert!(Cranks::new(None).is_empty());
    }
}

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
//! account the grader holds: the cheatcodes are locked by then. A crank that
//! fails ends the session as a grader error, since the venue is no longer
//! one the scenario describes.

use async_trait::async_trait;
use solana_rpc_client::rpc_client::RpcClient;

use crate::Result;

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

/// Upkeep for one venue.
#[async_trait]
pub trait VenueCrank: Send {
    fn name(&self) -> &str;

    /// Called once a step, after its blocks have landed.
    async fn crank(&mut self, rpc: &RpcClient, step: &CrankStep) -> Result<()>;
}

/// Makes a session's cranks. A session builds its own, so a rerun starts
/// from fresh state.
pub type CrankFactory = dyn Fn() -> Vec<Box<dyn VenueCrank>> + Send + Sync;

/// A session's cranks, in the order they run.
#[derive(Default)]
pub struct Cranks(Vec<Box<dyn VenueCrank>>);

impl Cranks {
    pub fn new(factory: Option<&CrankFactory>) -> Self {
        Self(factory.map(|make| make()).unwrap_or_default())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub async fn step(&mut self, rpc: &RpcClient, step: &CrankStep) -> Result<()> {
        for crank in &mut self.0 {
            crank.crank(rpc, step).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A crank that records the steps it was called at, and fails at one
    /// if asked to.
    pub(crate) struct Recorder {
        pub name: &'static str,
        pub calls: Arc<Mutex<Vec<(&'static str, CrankStep)>>>,
        pub fail_at: Option<u64>,
    }

    #[async_trait]
    impl VenueCrank for Recorder {
        fn name(&self) -> &str {
            self.name
        }

        async fn crank(&mut self, _rpc: &RpcClient, step: &CrankStep) -> Result<()> {
            self.calls.lock().unwrap().push((self.name, *step));
            if self.fail_at == Some(step.k) {
                return Err(crate::Error::Timeout(format!("{} crank", self.name)));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{testing::Recorder, *};

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

    #[tokio::test]
    async fn cranks_run_in_order_each_step() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let make = {
            let calls = calls.clone();
            move || -> Vec<Box<dyn VenueCrank>> {
                ["a", "b"]
                    .map(|name| {
                        Box::new(Recorder {
                            name,
                            calls: calls.clone(),
                            fail_at: None,
                        }) as Box<dyn VenueCrank>
                    })
                    .into()
            }
        };
        let mut cranks = Cranks::new(Some(&make));
        for k in 0..3 {
            cranks.step(&rpc(), &step(k)).await.unwrap();
        }
        let names: Vec<_> = calls.lock().unwrap().iter().map(|c| c.0).collect();
        assert_eq!(names, ["a", "b", "a", "b", "a", "b"]);
        assert_eq!(calls.lock().unwrap()[5].1, step(2));
    }

    #[tokio::test]
    async fn a_failing_crank_stops_the_step() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let make = {
            let calls = calls.clone();
            move || -> Vec<Box<dyn VenueCrank>> {
                vec![
                    Box::new(Recorder {
                        name: "a",
                        calls: calls.clone(),
                        fail_at: Some(1),
                    }),
                    Box::new(Recorder {
                        name: "b",
                        calls: calls.clone(),
                        fail_at: None,
                    }),
                ]
            }
        };
        let mut cranks = Cranks::new(Some(&make));
        cranks.step(&rpc(), &step(0)).await.unwrap();
        assert!(cranks.step(&rpc(), &step(1)).await.is_err());
        let names: Vec<_> = calls.lock().unwrap().iter().map(|c| c.0).collect();
        assert_eq!(names, ["a", "b", "a"], "b never ran at step 1");
    }

    #[test]
    fn no_factory_means_no_cranks() {
        assert!(Cranks::new(None).is_empty());
    }
}

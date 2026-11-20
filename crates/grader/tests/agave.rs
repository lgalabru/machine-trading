//! The plugin in a real Agave validator (`docs/rfd/0004`, step 5, and its
//! open question on Agave's plugin manager): `solana-test-validator` loads
//! it from `--geyser-plugin-config` at boot, its runtime is fed the
//! validator's slots, and it answers `mtx live` on its control socket.
//!
//! Needs `solana-test-validator` on the path and `MTX_TEST_GEYSER_PLUGIN`,
//! the plugin built with the validator's interface version (`just agave`).

use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use mtx_core::control::{self, Reply};
use serde_json::json;

/// Ports of the validator the test starts: away from a default one, and
/// from the TPU test's.
const RPC_PORT: u16 = 20_899;
const FAUCET_PORT: u16 = 21_900;
const GOSSIP_PORT: u16 = 20_001;
const DYNAMIC_PORTS: &str = "20002-20040";
/// How long the validator may take to boot and feed the plugin.
const WITHIN: Duration = Duration::from_secs(90);
/// Slots the runtime must have seen: the validator is producing, and the
/// plugin is fed.
const SLOTS_SEEN: u64 = 10;

/// The validator, stopped when the test ends however it ends.
struct Validator(Child);

impl Drop for Validator {
    fn drop(&mut self) {
        // Already exited is fine: nothing to stop.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator and the built plugin"]
async fn an_agave_validator_feeds_the_plugin() {
    let plugin = PathBuf::from(
        std::env::var("MTX_TEST_GEYSER_PLUGIN").expect("MTX_TEST_GEYSER_PLUGIN: the built plugin"),
    );
    let dir = std::env::temp_dir().join(format!("mtx-agave-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("control.sock");
    let config = dir.join("plugin.json");
    std::fs::write(
        &config,
        json!({
            "libpath": plugin,
            "mtx": {
                "journal": dir.join("journal.jsonl"),
                "markets": [],
                "control": {
                    "socket": socket,
                    "state": dir.join("strategies.json"),
                    "rpc_url": format!("http://127.0.0.1:{RPC_PORT}"),
                    "logs": dir.join("logs"),
                },
            },
        })
        .to_string(),
    )
    .unwrap();
    let _validator = Validator(
        Command::new("solana-test-validator")
            .args(["--reset", "--quiet", "--ledger"])
            .arg(dir.join("ledger"))
            .args(["--rpc-port", &RPC_PORT.to_string()])
            .args(["--faucet-port", &FAUCET_PORT.to_string()])
            .args(["--gossip-port", &GOSSIP_PORT.to_string()])
            .args(["--dynamic-port-range", DYNAMIC_PORTS])
            .arg("--geyser-plugin-config")
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("solana-test-validator on the path"),
    );
    let deadline = Instant::now() + WITHIN;
    let feed = loop {
        if let Ok(Reply::Ok { feed, .. }) = control::request(&socket, &control::Command::Status)
            && feed.slot >= SLOTS_SEEN
        {
            break feed;
        }
        assert!(
            Instant::now() < deadline,
            "the plugin wasn't fed {SLOTS_SEEN} slots"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(feed.dropped, 0, "{feed:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

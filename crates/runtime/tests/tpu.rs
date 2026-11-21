//! Step 5 of `docs/rfd/0004`: the live runtime's TPU client against a real
//! validator. It tracks the leaders from the slots and the node's RPC,
//! connects to them over QUIC, and a transfer sent through it lands with no
//! RPC send, within a few slots of being sent.
//!
//! Needs `solana-test-validator` on the path; the slots stand in for the
//! geyser's by polling the node, as the plugin isn't in this validator.

use std::{
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use mtx_core::feed::SlotStatus;
use mtx_kit::chain::{ChainClient, RpcChainClient};
use mtx_runtime::live::tpu::{self, Tpu, TpuChainClient, TpuConfig};
use solana_keypair::Keypair;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::{Transaction, versioned::VersionedTransaction};

/// Ports of the validator the test starts: away from a default one.
const RPC_PORT: u16 = 18_899;
const FAUCET_PORT: u16 = 19_900;
const GOSSIP_PORT: u16 = 18_001;
const DYNAMIC_PORTS: &str = "18002-18040";
/// How long the validator may take to answer, and a transfer to land.
const WITHIN: Duration = Duration::from_secs(60);
/// The most slots between sending a transfer and its landing: the current
/// leader's window and the next.
const LANDS_WITHIN_SLOTS: u64 = 8;

/// The validator, stopped when the test ends however it ends.
struct Validator(Child);

impl Drop for Validator {
    fn drop(&mut self) {
        // Already exited is fine: nothing to stop.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn eventually<T>(what: &str, mut attempt: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WITHIN;
    loop {
        if let Some(value) = attempt().await {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "{what} within {}s",
            WITHIN.as_secs()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Hand the node's slots to the leader tracker as the feed would: a start
/// when one appears, the end of the one before.
fn poll_slots(rpc: Arc<RpcClient>, events: tpu::SlotEvents) {
    tokio::spawn(async move {
        let mut last = 0;
        loop {
            if let Ok(slot) = rpc.get_slot().await
                && slot > last
            {
                events.offer(slot.saturating_sub(1), SlotStatus::Completed);
                events.offer(slot, SlotStatus::FirstShredReceived);
                last = slot;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator"]
async fn a_transfer_sent_over_the_tpu_lands_without_an_rpc_send() {
    let ledger = std::env::temp_dir().join(format!("mtx-tpu-ledger-{}", std::process::id()));
    let _validator = Validator(
        Command::new("solana-test-validator")
            .args(["--reset", "--quiet", "--ledger"])
            .arg(&ledger)
            .args(["--rpc-port", &RPC_PORT.to_string()])
            .args(["--faucet-port", &FAUCET_PORT.to_string()])
            .args(["--gossip-port", &GOSSIP_PORT.to_string()])
            .args(["--dynamic-port-range", DYNAMIC_PORTS])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("solana-test-validator on the path"),
    );
    let url = format!("http://127.0.0.1:{RPC_PORT}");
    // Confirmed: a balance read right after a status reads the same bank.
    let rpc = Arc::new(RpcClient::new_with_commitment(
        url.clone(),
        solana_commitment_config::CommitmentConfig::confirmed(),
    ));
    eventually("the validator answers", async || {
        rpc.get_slot().await.ok().filter(|&s| s > 2)
    })
    .await;

    let payer = Keypair::new();
    let airdrop = rpc
        .request_airdrop(&payer.pubkey(), 2_000_000_000)
        .await
        .unwrap();
    eventually("the airdrop lands", async || {
        rpc.confirm_transaction(&airdrop)
            .await
            .ok()
            .filter(|&ok| ok)
    })
    .await;

    let (events, slots) = tpu::slot_events();
    poll_slots(rpc.clone(), events);
    let tpu = Arc::new(
        Tpu::start(
            &TpuConfig {
                identity: None,
                fanout: 1,
                preflight: false,
            },
            &url,
            slots,
        )
        .await
        .expect("the TPU client starts"),
    );
    // A slot or two, for the tracker to know the leader and the client to
    // connect ahead of the send.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let transfer = |to: &Keypair, lamports: u64, blockhash| {
        VersionedTransaction::from(Transaction::new_signed_with_payer(
            &[solana_system_interface::instruction::transfer(
                &payer.pubkey(),
                &to.pubkey(),
                lamports,
            )],
            Some(&payer.pubkey()),
            &[&payer],
            blockhash,
        ))
    };

    // Straight to the TPU: no RPC carries it. Each lands; the first opens
    // the connection, so the rest are timed.
    let mut landed = Vec::new();
    for n in 0..4u64 {
        let to = Keypair::new();
        let blockhash = rpc.get_latest_blockhash().await.unwrap();
        let sent_at = rpc.get_slot().await.unwrap();
        let signature = tpu.send(&transfer(&to, 1_000_000 + n, blockhash)).unwrap();
        let landed_at = eventually("the transfer lands", async || {
            let statuses = rpc.get_signature_statuses(&[signature]).await.ok()?;
            let status = statuses.value.into_iter().next().flatten()?;
            assert_eq!(status.err, None, "the transfer failed");
            status
                .satisfies_commitment(solana_commitment_config::CommitmentConfig::confirmed())
                .then_some(status.slot)
        })
        .await;
        assert_eq!(rpc.get_balance(&to.pubkey()).await.unwrap(), 1_000_000 + n);
        landed.push((sent_at, landed_at));
    }
    eprintln!("sent and landed, by slot: {landed:?}");
    for &(sent_at, landed_at) in &landed[1..] {
        assert!(
            landed_at <= sent_at + LANDS_WITHIN_SLOTS,
            "sent at slot {sent_at}, landed at {landed_at}: {landed:?}"
        );
    }

    // Through the chain a desk trades on: sent over the TPU, confirmed.
    let events = mtx_runtime::live::events::Events::default();
    let mut heard = events.receiver();
    let chain = TpuChainClient::new(
        RpcChainClient::new(url.clone()),
        tpu.clone(),
        "test",
        Arc::default(),
        events,
    );
    let again = Keypair::new();
    let blockhash = chain.get_latest_blockhash().await.unwrap();
    chain
        .send_and_confirm(&transfer(&again, 2_000_000, blockhash))
        .await
        .expect("a confirmed send");
    assert_eq!(rpc.get_balance(&again.pubkey()).await.unwrap(), 2_000_000);

    // Fire and forget, as a live strategy sends: the send returns as it
    // leaves, and its fate comes on the event stream.
    let third = Keypair::new();
    let started = std::time::Instant::now();
    let signature = chain
        .send(&transfer(&third, 3_000_000, blockhash))
        .await
        .expect("sent");
    let sent_in = started.elapsed();
    assert!(
        sent_in < std::time::Duration::from_millis(100),
        "{sent_in:?}"
    );
    let fate = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let line = heard.recv().await.expect("the events");
            let line: mtx_core::events::Line = serde_json::from_str(&line).unwrap();
            if let mtx_core::events::Event::Fate { .. } = line.event {
                return line.event;
            }
        }
    })
    .await
    .expect("a fate within a minute");
    let mtx_core::events::Event::Fate {
        signature: fated,
        landed,
        slot,
        ..
    } = fate
    else {
        unreachable!("matched a fate");
    };
    assert_eq!(fated, signature.to_string());
    assert!(landed && slot.is_some());
    assert_eq!(rpc.get_balance(&third.pubkey()).await.unwrap(), 3_000_000);
    let _ = std::fs::remove_dir_all(&ledger);
}

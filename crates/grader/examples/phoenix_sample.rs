//! Samples a Phoenix perp market from a live chain, for working out how
//! its accounts make its top of book (`docs/rfd/0004`): each sample is
//! Hawkeye's `view_bbo` and every account the view reads, as one
//! simulation saw them, so the inputs and the output are of one state.
//!
//! ```sh
//! cargo run -p mtx-grader --example phoenix_sample -- SOL 20 target/phoenix-samples
//! ```
//!
//! `MTX_SAMPLE_RPC_URL` is the node (mainnet's public endpoint if unset).
//! Each sample is a directory named by its slot: `view.json`, and one
//! `<address>.bin` an account.

use std::{path::PathBuf, time::Duration};

use base64::Engine as _;
use mtx_kit::phoenix_rise::{
    ix::hawkeye::{ViewBboReturn, decode_hawkeye_return},
    types::exchange::ExchangeResponse,
};
use serde_json::{Value, json};
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;

const PUBLIC_RPC: &str = "https://api.mainnet-beta.solana.com";
/// An account that exists and holds lamports: a simulation needs a payer,
/// and signs nothing. Phoenix's own client uses the same one.
const PAYER: Pubkey = solana_pubkey::pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
/// Between samples: a few slots, so the market has moved.
const EVERY: Duration = Duration::from_secs(3);
const SAMPLES_MAX: usize = 1_000;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let symbol = args
        .next()
        .ok_or("usage: phoenix_sample SYMBOL COUNT DIR")?;
    let count: usize = args.next().ok_or("COUNT")?.parse()?;
    let dir = PathBuf::from(args.next().ok_or("DIR")?).join(&symbol);
    assert!(count <= SAMPLES_MAX, "at most {SAMPLES_MAX} samples a run");
    let url = std::env::var("MTX_SAMPLE_RPC_URL").unwrap_or_else(|_| PUBLIC_RPC.into());
    std::fs::create_dir_all(&dir)?;

    let exchange: ExchangeResponse =
        mtx_grader::phoenix::fetch_exchange(mtx_grader::phoenix::DEFAULT_API_URL).await?;
    std::fs::write(dir.join("exchange.json"), serde_json::to_string(&exchange)?)?;
    let phoenix = mtx_kit::Phoenix::new(exchange, &url);
    let mut instructions = phoenix.tx().build_hawkeye_view_bbo(&symbol)?;
    assert_eq!(instructions.len(), 1, "one view, one instruction");
    let view = instructions.remove(0);
    // Everything the view reads, the program itself aside.
    let addresses: Vec<String> = view.accounts.iter().map(|a| a.pubkey.to_string()).collect();
    // The compute budget program's SetComputeUnitLimit, at the most a
    // transaction may ask: the view walks every spline and its maker.
    let budget = solana_transaction::Instruction {
        program_id: solana_pubkey::pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: Vec::new(),
        data: [&[2u8][..], &1_400_000u32.to_le_bytes()].concat(),
    };
    let transaction = Transaction::new_with_payer(&[budget, view], Some(&PAYER));
    let wire = base64::engine::general_purpose::STANDARD.encode(wincode::serialize(&transaction)?);
    let http = reqwest::Client::new();

    for n in 0..count {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "simulateTransaction", "params": [
            wire,
            { "sigVerify": false, "replaceRecentBlockhash": true, "commitment": "processed",
              "encoding": "base64",
              "accounts": { "encoding": "base64", "addresses": addresses } },
        ]});
        let answer: Value = http.post(&url).json(&body).send().await?.json().await?;
        let result = &answer["result"];
        if !result["value"]["err"].is_null() || result.is_null() {
            eprintln!(
                "sample {n}: {}",
                answer.to_string().chars().take(400).collect::<String>()
            );
            tokio::time::sleep(EVERY).await;
            continue;
        }
        let slot = result["context"]["slot"].as_u64().ok_or("no slot")?;
        let decode = |text: &Value| {
            base64::engine::general_purpose::STANDARD.decode(text.as_str().unwrap_or_default())
        };
        let returned = decode(&result["value"]["returnData"]["data"][0])?;
        let bbo: ViewBboReturn = decode_hawkeye_return(&returned, "view_bbo")?;
        let sample = dir.join(slot.to_string());
        std::fs::create_dir_all(&sample)?;
        let accounts = result["value"]["accounts"]
            .as_array()
            .ok_or("no accounts")?;
        assert_eq!(accounts.len(), addresses.len(), "one account an address");
        for (address, account) in addresses.iter().zip(accounts) {
            std::fs::write(
                sample.join(format!("{address}.bin")),
                decode(&account["data"][0])?,
            )?;
        }
        std::fs::write(
            sample.join("view.json"),
            json!({
                "slot": slot,
                "symbol": symbol,
                "best_bid_ticks": bbo.best_bid_ticks(),
                "best_ask_ticks": bbo.best_ask_ticks(),
                "mark_price_ticks": bbo.mark_price_ticks,
                "index_price_ticks": bbo.index_price_ticks,
                "mark_price_last_updated_slot": bbo.mark_price_last_updated_slot,
                "index_price_last_updated_slot": bbo.index_price_last_updated_slot,
                "accounts": addresses,
            })
            .to_string(),
        )?;
        println!(
            "{n}: slot {slot} bid {:?} ask {:?} mark {} index {}",
            bbo.best_bid_ticks(),
            bbo.best_ask_ticks(),
            bbo.mark_price_ticks,
            bbo.index_price_ticks
        );
        tokio::time::sleep(EVERY).await;
    }
    Ok(())
}

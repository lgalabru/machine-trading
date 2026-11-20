//! Asks Phoenix's Hawkeye `view_bbo` about a market a `phoenix_sample`
//! sample holds, with its splines edited: how the uncrossing rule and the
//! base widths in `mtx_kit::venue::perp_top_of_book` were found
//! (`docs/rfd/0004`). One input changed at a time, and the view asked again.
//!
//! ```sh
//! surfpool start --rpc-url https://api.mainnet-beta.solana.com --port 18899 --no-tui &
//! cargo run -p mtx-grader --example phoenix_probe -- http://localhost:18899 \
//!     target/phoenix-samples/SOL/471819296 4UN.is_active=0 DUG.bid0.start=0
//! ```
//!
//! The surfnet forks mainnet, for Phoenix's program. The sample's accounts
//! are written into it with every slot they hold moved to the surfnet's
//! clock, so that each oracle, crank and spline keeps its age (a spline's
//! regions live a few slots). An edit picks a spline by its trader's base58
//! prefix:
//!
//! - `PREFIX.FIELD=N` sets a spline's field, `=+N` and `=-N` move it;
//! - `PREFIX.bidK.FIELD=N` and `PREFIX.askK.FIELD=N` a region's;
//! - `copy:A:B:FROM:TO` copies bytes `FROM..TO` of spline B over A's;
//! - `trader:A:B` gives spline A the trader of spline B;
//! - `swap:A:B` swaps the two splines' places in the collection.

use std::path::PathBuf;

use base64::Engine as _;
use mtx_kit::phoenix_rise::{
    ix::hawkeye::{ViewBboReturn, decode_hawkeye_return},
    types::exchange::ExchangeResponse,
};
use serde_json::{Value, json};
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;

/// An account that exists and holds lamports: a simulation needs a payer,
/// and signs nothing. Phoenix's own client uses the same one.
const PAYER: Pubkey = solana_pubkey::pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
/// The spline collection's layout (`phoenix-rise-accounts` 0.10.2,
/// `spline_collection.rs`): a header, then the splines, each with its bid
/// and then its ask regions from byte 120.
const HEADER_LEN: usize = 112;
const SPLINE_LEN: usize = 1336;
const REGION_LEN: usize = 48;
const BID_REGIONS_AT: usize = 120;
const ASK_REGIONS_AT: usize = 600;
const REGIONS_PER_SIDE: usize = 10;
const NUM_SPLINES_AT: usize = 72;
/// A stored slot is a u64 no further than this before the sample's slot:
/// about two epochs, older than any oracle, crank or spline update a live
/// market keeps.
const SLOT_AGE_MAX: u64 = 2_000_000;
/// The edits a probe takes at most.
const EDITS_MAX: usize = 64;

/// A spline field's byte offset and width.
fn spline_field(name: &str) -> Result<(usize, usize), String> {
    Ok(match name {
        "is_active" => (32, 1),
        "mid" => (40, 8),
        "seq" => (48, 8),
        "seq_slot" => (56, 8),
        "update_slot" => (64, 8),
        "bid_offset" => (72, 8),
        "ask_offset" => (80, 8),
        "bid_filled" => (88, 8),
        "ask_filled" => (96, 8),
        "bid_num" => (104, 8),
        "ask_num" => (112, 8),
        "price_seq" => (1080, 8),
        "param_seq" => (1088, 8),
        "flags" => (1096, 4),
        "leverage_decrease_bps" => (1100, 4),
        "max_long" => (1104, 4),
        "max_short" => (1108, 4),
        "bid_base_width" => (1112, 8),
        "ask_base_width" => (1120, 8),
        _ => return Err(format!("no spline field {name}")),
    })
}

/// A region field's byte offset and width.
fn region_field(name: &str) -> Result<(usize, usize), String> {
    Ok(match name {
        "start" => (0, 8),
        "end" => (8, 8),
        "density" => (16, 4),
        "hidden" => (20, 4),
        "total" => (24, 8),
        "filled" => (32, 4),
        "hidden_filled" => (36, 4),
        "lifespan" => (40, 8),
        _ => return Err(format!("no region field {name}")),
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .ok_or("usage: phoenix_probe URL SAMPLE_DIR [EDIT ...]")?;
    let sample = PathBuf::from(args.next().ok_or("SAMPLE_DIR")?);
    let edits: Vec<String> = args.collect();
    if edits.len() > EDITS_MAX {
        return Err(format!("at most {EDITS_MAX} edits").into());
    }
    let view: Value = serde_json::from_str(&std::fs::read_to_string(sample.join("view.json"))?)?;
    let exchange: ExchangeResponse = serde_json::from_str(&std::fs::read_to_string(
        sample
            .parent()
            .ok_or("a sample is in a market's directory")?
            .join("exchange.json"),
    )?)?;
    let symbol = view["symbol"].as_str().ok_or("no symbol")?.to_string();
    let market = exchange
        .markets
        .iter()
        .find(|m| m.symbol == symbol)
        .ok_or("no market")?
        .clone();
    let phoenix = mtx_kit::Phoenix::new(exchange, &url);
    let view_ix = phoenix.tx().build_hawkeye_view_bbo(&symbol)?.remove(0);
    let addresses: Vec<String> = view_ix
        .accounts
        .iter()
        .map(|a| a.pubkey.to_string())
        .collect();
    let budget = solana_transaction::Instruction {
        program_id: solana_pubkey::pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: Vec::new(),
        data: [&[2u8][..], &1_400_000u32.to_le_bytes()].concat(),
    };
    let transaction = Transaction::new_with_payer(&[budget, view_ix], Some(&PAYER));
    let wire = base64::engine::general_purpose::STANDARD.encode(wincode::serialize(&transaction)?);
    let rpc = Rpc {
        http: reqwest::Client::new(),
        url,
    };

    // The view's clock is the bank's: the processed slot, not the default
    // commitment's.
    let now = rpc
        .call("getSlot", json!([{"commitment": "processed"}]))
        .await?
        .as_u64()
        .ok_or("no slot")?;
    let sampled = view["slot"].as_u64().ok_or("no slot in view.json")?;
    for address in &addresses {
        let path = sample.join(format!("{address}.bin"));
        if !path.exists() {
            continue;
        }
        let mut data = std::fs::read(&path)?;
        shift_slots(&mut data, sampled, now);
        if *address == market.spline_pubkey {
            for edit in &edits {
                apply(&mut data, edit)?;
            }
            print_splines(&data);
        }
        rpc.set_account(address, &data).await?;
    }
    let answer = rpc
        .call(
            "simulateTransaction",
            json!([wire, {"sigVerify": false, "replaceRecentBlockhash": true, "encoding": "base64"}]),
        )
        .await?;
    let result = &answer["value"];
    if !result["err"].is_null() {
        return Err(format!("the view failed: {}", result["logs"]).into());
    }
    let returned = base64::engine::general_purpose::STANDARD
        .decode(result["returnData"]["data"][0].as_str().unwrap_or_default())?;
    let bbo: ViewBboReturn = decode_hawkeye_return(&returned, "view_bbo")?;
    println!(
        "view: bid {:?} ask {:?} mark {} index {} | sampled: bid {} ask {}",
        bbo.best_bid_ticks(),
        bbo.best_ask_ticks(),
        bbo.mark_price_ticks,
        bbo.index_price_ticks,
        view["best_bid_ticks"],
        view["best_ask_ticks"],
    );
    Ok(())
}

struct Rpc {
    http: reqwest::Client,
    url: String,
}

impl Rpc {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let answer: Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("{method}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("{method}: {e}"))?;
        if !answer["error"].is_null() {
            return Err(format!("{method}: {}", answer["error"]));
        }
        Ok(answer["result"].clone())
    }

    /// Write `data` at `address`, keeping the owner and lamports the fork
    /// has for it.
    async fn set_account(&self, address: &str, data: &[u8]) -> Result<(), String> {
        let info = self
            .call("getAccountInfo", json!([address, {"encoding": "base64"}]))
            .await?;
        let owner = info["value"]["owner"]
            .as_str()
            .ok_or("no owner")?
            .to_string();
        let lamports = info["value"]["lamports"].as_u64().ok_or("no lamports")?;
        let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
        self.call(
            "surfnet_setAccount",
            json!([address, {"lamports": lamports, "owner": owner, "executable": false, "data": hex}]),
        )
        .await
        .map(|_| ())
    }
}

/// Move every slot `data` holds from the sample's clock to the surfnet's:
/// any aligned u64 within [`SLOT_AGE_MAX`] before the sampled slot.
fn shift_slots(data: &mut [u8], sampled: u64, now: u64) {
    assert!(now >= sampled, "the fork is ahead of the sample");
    for at in (0..data.len().saturating_sub(7)).step_by(8) {
        let value = read(data, at, 8);
        if value.saturating_add(SLOT_AGE_MAX) >= sampled && value <= sampled {
            write(data, at, 8, value + (now - sampled));
        }
    }
}

fn apply(data: &mut [u8], edit: &str) -> Result<(), String> {
    if let Some(rest) = edit.strip_prefix("copy:") {
        let parts: Vec<&str> = rest.split(':').collect();
        let [to_spline, from_spline, from, to] = parts.as_slice() else {
            return Err(format!("{edit}: copy:A:B:FROM:TO"));
        };
        let (from, to): (usize, usize) = (number(from)?, number(to)?);
        if from >= to || to > SPLINE_LEN {
            return Err(format!("{edit}: bytes within a spline"));
        }
        let (a, b) = (find(data, to_spline)?, find(data, from_spline)?);
        data.copy_within(b + from..b + to, a + from);
        return Ok(());
    }
    if let Some(rest) = edit.strip_prefix("trader:") {
        let (to_spline, from_spline) = rest.split_once(':').ok_or("trader:A:B")?;
        let (a, b) = (find(data, to_spline)?, find(data, from_spline)?);
        data.copy_within(b..b + 32, a);
        return Ok(());
    }
    if let Some(rest) = edit.strip_prefix("swap:") {
        let (x, y) = rest.split_once(':').ok_or("swap:A:B")?;
        let (a, b) = (find(data, x)?, find(data, y)?);
        let spline_a = data[a..a + SPLINE_LEN].to_vec();
        data.copy_within(b..b + SPLINE_LEN, a);
        data[b..b + SPLINE_LEN].copy_from_slice(&spline_a);
        return Ok(());
    }
    let (path, value) = edit
        .split_once('=')
        .ok_or_else(|| format!("{edit}: PREFIX.FIELD=N"))?;
    let parts: Vec<&str> = path.split('.').collect();
    let (spline, (offset, len)) = match parts.as_slice() {
        [spline, field] => (*spline, spline_field(field)?),
        [spline, region, field] => {
            let (side, index) = match (region.strip_prefix("bid"), region.strip_prefix("ask")) {
                (Some(index), _) => (BID_REGIONS_AT, number(index)?),
                (_, Some(index)) => (ASK_REGIONS_AT, number(index)?),
                _ => return Err(format!("{edit}: bidK or askK")),
            };
            if index >= REGIONS_PER_SIDE {
                return Err(format!("{edit}: a side has {REGIONS_PER_SIDE} regions"));
            }
            let (field_at, len) = region_field(field)?;
            (*spline, (side + index * REGION_LEN + field_at, len))
        }
        _ => return Err(format!("{edit}: PREFIX.FIELD or PREFIX.REGION.FIELD")),
    };
    let at = find(data, spline)? + offset;
    let current = read(data, at, len);
    let new = match (value.strip_prefix('+'), value.strip_prefix('-')) {
        (Some(up), _) => current.checked_add(number(up)? as u64),
        (_, Some(down)) => current.checked_sub(number(down)? as u64),
        _ => Some(number(value)? as u64),
    }
    .ok_or_else(|| format!("{edit}: out of range"))?;
    write(data, at, len, new);
    Ok(())
}

fn number(text: &str) -> Result<usize, String> {
    text.parse().map_err(|_| format!("{text} is not a number"))
}

fn read(data: &[u8], at: usize, len: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes[..len].copy_from_slice(&data[at..at + len]);
    u64::from_le_bytes(bytes)
}

fn write(data: &mut [u8], at: usize, len: usize, value: u64) {
    assert!(
        len == 8 || value < 1 << (8 * len),
        "{value} fits {len} bytes"
    );
    data[at..at + len].copy_from_slice(&value.to_le_bytes()[..len]);
}

/// Each spline's offset in the collection, and its trader in base58.
fn splines(data: &[u8]) -> Vec<(usize, String)> {
    let count = read(data, NUM_SPLINES_AT, 4) as usize;
    assert!(
        data.len() >= HEADER_LEN + count * SPLINE_LEN,
        "a whole collection"
    );
    (0..count)
        .map(|index| {
            let at = HEADER_LEN + index * SPLINE_LEN;
            let mut trader = [0u8; 32];
            trader.copy_from_slice(&data[at..at + 32]);
            (at, Pubkey::new_from_array(trader).to_string())
        })
        .collect()
}

fn find(data: &[u8], prefix: &str) -> Result<usize, String> {
    let mut found = splines(data)
        .into_iter()
        .filter(|(_, trader)| trader.starts_with(prefix));
    let (at, _) = found.next().ok_or_else(|| format!("no spline {prefix}"))?;
    if found.next().is_some() {
        return Err(format!("more than one spline {prefix}"));
    }
    Ok(at)
}

/// Every spline with a mid, as the edits left it.
fn print_splines(data: &[u8]) {
    for (at, trader) in splines(data) {
        let spline = &data[at..at + SPLINE_LEN];
        let field = |name: &str| {
            let (offset, len) = spline_field(name).expect("a known field");
            read(spline, offset, len)
        };
        if field("mid") == 0 {
            continue;
        }
        println!(
            "{} active {} mid {} updated {} width {}/{} limits {}/{} (flags {})",
            &trader[..6],
            field("is_active"),
            field("mid"),
            field("update_slot"),
            field("bid_base_width"),
            field("ask_base_width"),
            field("max_long"),
            field("max_short"),
            field("flags"),
        );
        let sides = [
            ("bid", BID_REGIONS_AT, field("bid_offset"), field("bid_num")),
            ("ask", ASK_REGIONS_AT, field("ask_offset"), field("ask_num")),
        ];
        for (side, regions_at, first, end) in sides {
            for index in first as usize..(end as usize).min(REGIONS_PER_SIDE) {
                let region = &spline[regions_at + index * REGION_LEN..][..REGION_LEN];
                let value = |name: &str| {
                    let (offset, len) = region_field(name).expect("a known field");
                    read(region, offset, len)
                };
                println!(
                    "    {side}{index} [{}, {}) {} lots a tick, {} of {} filled, life {}",
                    value("start"),
                    value("end"),
                    value("density"),
                    value("filled"),
                    value("total"),
                    value("lifespan"),
                );
            }
        }
    }
}

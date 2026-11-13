//! The router smoke, as a process bot: Jupiter's API at the session's RPC
//! URL. On its first turn it asks `GET /quote` for a sale of 200 of the
//! base for USDC, then `POST /swap-instructions` for it, and signs and
//! sends the route as one v0 transaction naming the router's lookup
//! table. On its third it does the same for a purchase with 2,000 USDC
//! through the JSON-RPC methods, with the quote's least out doubled, which
//! no route pays: the program refuses it. On its fifth it asks for
//! instructions through a pool the router doesn't route through, which the
//! router refuses. What happened is logged as `probe:` lines, which the
//! check reads.

use std::{
    io::{Read, Write},
    net::TcpStream,
};

use base64::Engine as _;
use mtx_kit::{
    BotEnv, Instruction, Keypair, Pubkey, RpcClient, Signature, Signer, Turns, rpc_client,
};
use serde_json::{Value, json};
use solana_message::{AddressLookupTableAccount, VersionedMessage, v0};
use solana_rpc_client_api::request::RpcRequest;
use solana_transaction::{AccountMeta, versioned::VersionedTransaction};

const SPCX: &str = "SPCXxcqXj6e5dJDVNovHN8744zkbhM2bYudU45BimGb";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// One HTTP/1.1 request to the session's endpoint: the status and the body.
fn http(url: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
    let host = url.trim_start_matches("http://").trim_end_matches('/');
    let mut stream = TcpStream::connect(host).map_err(|e| e.to_string())?;
    let body = body.map(Value::to_string).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .map_err(|e| e.to_string())?;
    let (head, rest) = answer.split_once("\r\n\r\n").ok_or("no HTTP answer")?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("no HTTP status")?;
    // A chunked body: its chunks, sizes dropped.
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut out = String::new();
        let mut rest = rest;
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let size = usize::from_str_radix(size.trim(), 16).map_err(|e| e.to_string())?;
            if size == 0 {
                break;
            }
            out.push_str(&tail[..size]);
            rest = &tail[size + 2..];
        }
        out
    } else {
        rest.to_string()
    };
    Ok((status, serde_json::from_str(&body).unwrap_or(Value::Null)))
}

fn instruction(value: &Value) -> Result<Instruction, String> {
    let key = |v: &Value| -> Result<Pubkey, String> {
        v.as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("not an address: {v}"))
    };
    Ok(Instruction {
        program_id: key(&value["programId"])?,
        accounts: value["accounts"]
            .as_array()
            .ok_or("no accounts")?
            .iter()
            .map(|a| {
                Ok(AccountMeta {
                    pubkey: key(&a["pubkey"])?,
                    is_signer: a["isSigner"].as_bool().unwrap_or(false),
                    is_writable: a["isWritable"].as_bool().unwrap_or(false),
                })
            })
            .collect::<Result<_, String>>()?,
        data: base64::engine::general_purpose::STANDARD
            .decode(value["data"].as_str().ok_or("no data")?)
            .map_err(|e| e.to_string())?,
    })
}

/// Sign and send the transaction a `/swap-instructions` answer describes.
fn send(rpc: &RpcClient, payer: &Keypair, built: &Value) -> Result<Signature, String> {
    let mut instructions = Vec::new();
    for part in [
        "computeBudgetInstructions",
        "setupInstructions",
        "swapInstructions",
    ] {
        for ix in built[part].as_array().ok_or(format!("no {part}"))? {
            instructions.push(instruction(ix)?);
        }
    }
    let mut tables = Vec::new();
    for address in built["addressLookupTableAddresses"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let key: Pubkey = address
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or("a bad table")?;
        let account = rpc.get_account(&key).map_err(|e| e.to_string())?;
        let table = mtx_kit::chain::lookup::decode(&key, &account.owner, &account.data)
            .map_err(|e| e.to_string())?;
        tables.push(AddressLookupTableAccount {
            key: table.key,
            addresses: table.addresses,
        });
    }
    let blockhash = rpc.get_latest_blockhash().map_err(|e| e.to_string())?;
    let message = v0::Message::try_compile(&payer.pubkey(), &instructions, &tables, blockhash)
        .map_err(|e| e.to_string())?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[payer])
        .map_err(|e| e.to_string())?;
    rpc.send_transaction(&tx).map_err(|e| e.to_string())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let payer = env.keypair()?;
    let rpc = rpc_client(&env.rpc_url);
    let user = payer.pubkey().to_string();
    let mut sent: Vec<Signature> = Vec::new();
    while let Some(step) = turns.next()? {
        for signature in std::mem::take(&mut sent) {
            match rpc.get_signature_statuses(&[signature])?.value[0].clone() {
                Some(status) => match status.err {
                    None => eprintln!("probe: landed ok {signature}"),
                    Some(error) => eprintln!("probe: failed {signature} {error}"),
                },
                None => sent.push(signature),
            }
        }
        match step.k {
            0 => {
                let path = format!(
                    "/quote?inputMint={SPCX}&outputMint={USDC}&amount=200000000&slippageBps=100"
                );
                let (status, quote) = http(&env.rpc_url, "GET", &path, None)?;
                eprintln!(
                    "probe: quote {status} out {} legs {}",
                    quote["outAmount"],
                    quote["routePlan"].as_array().map_or(0, Vec::len)
                );
                let body = json!({ "quoteResponse": quote, "userPublicKey": user });
                let (status, built) =
                    http(&env.rpc_url, "POST", "/swap-instructions", Some(&body))?;
                eprintln!(
                    "probe: swap-instructions {status} swaps {}",
                    built["swapInstructions"].as_array().map_or(0, Vec::len)
                );
                match send(&rpc, &payer, &built) {
                    Ok(signature) => {
                        eprintln!("probe: sent {signature}");
                        sent.push(signature);
                    }
                    Err(e) => eprintln!("probe: not sent {e}"),
                }
            }
            2 => {
                let params =
                    json!([{ "inputMint": USDC, "outputMint": SPCX, "amount": "2000000000" }]);
                let mut quote: Value = rpc.send(
                    RpcRequest::Custom {
                        method: "mtx_routerQuote",
                    },
                    params,
                )?;
                let out: u64 = quote["outAmount"].as_str().unwrap_or("0").parse()?;
                quote["otherAmountThreshold"] = json!((out * 2).to_string());
                let built: Value = rpc.send(
                    RpcRequest::Custom {
                        method: "mtx_routerSwapInstructions",
                    },
                    json!([{ "quoteResponse": quote, "userPublicKey": user }]),
                )?;
                match send(&rpc, &payer, &built) {
                    Ok(signature) => {
                        eprintln!("probe: sent greedy {signature}");
                        sent.push(signature);
                    }
                    Err(e) => eprintln!("probe: greedy not sent {e}"),
                }
            }
            4 => {
                let path = format!("/quote?inputMint={SPCX}&outputMint={USDC}&amount=1000000");
                let (_, mut quote) = http(&env.rpc_url, "GET", &path, None)?;
                quote["routePlan"][0]["swapInfo"]["ammKey"] =
                    json!(Pubkey::new_unique().to_string());
                let body = json!({ "quoteResponse": quote, "userPublicKey": user });
                let (status, refused) =
                    http(&env.rpc_url, "POST", "/swap-instructions", Some(&body))?;
                eprintln!("probe: foreign pool {status} {}", refused["error"]);
            }
            _ => {}
        }
    }
    Ok(())
}

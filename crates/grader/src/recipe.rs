//! Snapshot recipes: a snapshot's `recipe.toml` says what it holds and how to
//! build it, and [`build`] forks, warms up and exports it.
//!
//! ```toml
//! name = "sol-usdc-spot-2026-11-11"
//! kind = "spot"
//! symbol = "SOL-USDC"
//!
//! [[program]]                 # fetched into the fork and checked executable
//! name = "phoenix-v1"
//! address = "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY"
//!
//! [[account]]                 # fetched; a market, mint, oracle...
//! name = "phoenix-v1 SOL/USDC"
//! kind = "market"
//! address = "4DoNfFBfF7UokCC2FQzriy7yHK6DY6NVdYpuekQ5pRgg"
//!
//! [[warmup]]                  # in order
//! action = "spot-exercise"
//! ```
//!
//! The export has to hold every declared program and account, so a recipe
//! can't name something its warm-up never loads. The manifest records the
//! recipe's hash, the fork slot, every declared source and, when the recipe
//! made any, the simulated assets (see [`crate::simulated`]).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use mtx_core::Decimal;
use phoenix_rise::{core::LimitOrderTicket, math::Side};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use surfpool_sdk::{BlockProductionMode, Surfnet};
use tracing::info;

use crate::{
    Error, Result,
    chain::{self, Override},
    error::io_error,
    phoenix::{self, Phoenix},
    simulated::{self, MintSpec, OpenbookMarketSpec, PhoenixMarketSpec, PriceInput},
    snapshot::{
        self, Access, Created, MANIFEST_FILE, Manifest, SLOT_TIME_MS, Source, host_of,
        start_surfnet, write_gz, write_json,
    },
};

pub const RECIPE_FILE: &str = "recipe.toml";
/// The public RPC the repo forks by default.
pub const DEFAULT_FORK_RPC: &str = "https://api.mainnet-beta.solana.com";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub name: String,
    /// `spot` for a spot snapshot; absent for a Phoenix Eternal one.
    pub kind: Option<String>,
    pub symbol: String,
    #[serde(default)]
    pub fork: Fork,
    #[serde(default, rename = "program")]
    pub programs: Vec<Declared>,
    #[serde(default, rename = "account")]
    pub accounts: Vec<Declared>,
    #[serde(default, rename = "warmup")]
    pub warmup: Vec<Action>,
}

/// Where the chain state comes from: mainnet, or another snapshot (for a
/// recipe that only adds simulated assets, and for tests).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fork {
    /// A snapshot directory, relative to the recipe, started offline instead
    /// of forking.
    pub from_snapshot: Option<PathBuf>,
}

/// A program or account the snapshot must hold.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declared {
    pub name: String,
    pub address: String,
    /// `market`, `pool`, `mint`, `oracle`, `vault`... Free text for the
    /// manifest.
    #[serde(default)]
    pub kind: Option<String>,
}

/// What a `phoenix-eternal` action trades.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EternalSpec {
    pub symbol: String,
    #[serde(default = "default_phoenix_api")]
    pub phoenix_api: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Action {
    /// Trade a Phoenix Eternal market as a task does, with a throwaway trader.
    PhoenixEternal(EternalSpec),
    /// Trade SOL/USDC on Phoenix v1 and OpenBook v2 as a spot session does.
    SpotExercise,
    /// A simulated mint.
    CreateMint(MintSpec),
    /// A simulated Phoenix v1 market, seeded around `price`.
    #[serde(rename = "phoenix-v1-market")]
    PhoenixV1Market(PhoenixMarketSpec),
    /// A simulated OpenBook v2 market, seeded around `price`.
    #[serde(rename = "openbook-v2-market")]
    OpenbookV2Market(OpenbookMarketSpec),
}

fn default_phoenix_api() -> String {
    phoenix::DEFAULT_API_URL.into()
}

pub struct BuildOptions {
    /// Where the snapshot is written; the recipe's own directory by default.
    pub out_dir: Option<PathBuf>,
    /// The datasource to fork. May embed an API key; only its host is recorded.
    pub fork_rpc_url: String,
}

impl Recipe {
    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| invalid(format!("recipe: {e}")))
    }

    fn validate(&self) -> Result<()> {
        let mut names = std::collections::BTreeSet::new();
        for d in self.programs.iter().chain(&self.accounts) {
            parse_key(&d.address)?;
            if !names.insert(d.name.as_str()) {
                return Err(invalid(format!("{:?} is declared twice", d.name)));
            }
        }
        Ok(())
    }
}

fn invalid(reason: String) -> Error {
    Error::Transaction {
        label: "snapshot recipe".into(),
        reason,
    }
}

fn parse_key(text: &str) -> Result<Pubkey> {
    Pubkey::from_str(text).map_err(|e| invalid(format!("{text:?} is not an address: {e}")))
}

/// The recipe file's SHA-256, hex.
pub fn recipe_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A number in a TOML document at a dotted key.
pub fn toml_number(text: &str, key: &str) -> Result<f64> {
    let doc = toml::from_str::<toml::Value>(text)
        .map_err(|e| invalid(format!("calibration file: {e}")))?;
    let mut at = &doc;
    for part in key.split('.') {
        at = at
            .get(part)
            .ok_or_else(|| invalid(format!("calibration has no {key:?}")))?;
    }
    at.as_float()
        .or_else(|| at.as_integer().map(|i| i as f64))
        .ok_or_else(|| invalid(format!("calibration {key:?} is not a number")))
}

fn resolve_price(input: &PriceInput, dir: &Path) -> Result<Decimal> {
    let price = match (input.value, &input.calibration, &input.key) {
        (Some(v), None, None) => v,
        (None, Some(file), Some(key)) => {
            let path = dir.join(file);
            let text = std::fs::read_to_string(&path).map_err(io_error(&path))?;
            toml_number(&text, key)?
        }
        _ => {
            return Err(invalid(
                "a price is `value`, or `calibration` with `key`".into(),
            ));
        }
    };
    if !(price.is_finite() && price > 0.0) {
        return Err(invalid(format!("price {price} is not positive")));
    }
    Decimal::from_str(&format!("{price:.9}")).map_err(|e| invalid(format!("price {price}: {e}")))
}

/// What a build knows while its actions run.
struct World<'a> {
    recipe: &'a Recipe,
    dir: &'a Path,
    surfnet: &'a Surfnet,
    rpc: RpcClient,
    /// Simulated mints by the recipe's name for them.
    mints: BTreeMap<String, Pubkey>,
    created: Vec<Created>,
    exchange: Option<phoenix_rise::types::exchange::ExchangeResponse>,
    /// The last price an action reported.
    price: Option<f64>,
}

impl World<'_> {
    /// A mint named by an earlier `create-mint`, a declared account, or an address.
    fn mint(&self, reference: &str) -> Result<Pubkey> {
        if let Some(mint) = self.mints.get(reference) {
            return Ok(*mint);
        }
        if let Some(declared) = self.recipe.accounts.iter().find(|a| a.name == reference) {
            return parse_key(&declared.address);
        }
        parse_key(reference)
    }

    fn record(
        &mut self,
        kind: &str,
        name: &str,
        address: Pubkey,
        program: Pubkey,
        detail: serde_json::Value,
    ) {
        self.created.push(Created {
            kind: kind.into(),
            name: name.into(),
            address: address.to_string(),
            program: program.to_string(),
            simulated: true,
            detail,
        });
    }

    async fn run(&mut self, action: &Action) -> Result<()> {
        match action {
            Action::PhoenixEternal(spec) => {
                let (mark, exchange) =
                    phoenix_eternal(self.surfnet, &self.rpc, &spec.symbol, &spec.phoenix_api)
                        .await?;
                self.price = Some(mark);
                self.exchange = Some(exchange);
            }
            Action::SpotExercise => {
                self.price = Some(spot_exercise(self.surfnet, &self.rpc).await?);
            }
            Action::CreateMint(spec) => {
                let address = simulated::create_mint(self.surfnet, &self.recipe.name, spec)?;
                self.mints.insert(spec.name.clone(), address);
                self.record(
                    "mint",
                    &spec.name,
                    address,
                    spec.program.program(),
                    json!({ "decimals": spec.decimals }),
                );
            }
            Action::PhoenixV1Market(spec) => {
                let price = resolve_price(&spec.price, self.dir)?;
                let made = simulated::phoenix_v1_market(
                    self.surfnet,
                    &self.rpc,
                    &self.recipe.name,
                    spec,
                    self.mint(&spec.base)?,
                    self.mint(&spec.quote)?,
                    price,
                )
                .await?;
                self.price = price.to_string().parse().ok().or(self.price);
                self.record(
                    "phoenix-v1-market",
                    &spec.name,
                    made.address,
                    made.program,
                    made.detail,
                );
            }
            Action::OpenbookV2Market(spec) => {
                let price = resolve_price(&spec.price, self.dir)?;
                let made = simulated::openbook_v2_market(
                    self.surfnet,
                    &self.rpc,
                    &self.recipe.name,
                    spec,
                    self.mint(&spec.base)?,
                    self.mint(&spec.quote)?,
                    price,
                )
                .await?;
                self.price = price.to_string().parse().ok().or(self.price);
                self.record(
                    "openbook-v2-market",
                    &spec.name,
                    made.address,
                    made.program,
                    made.detail,
                );
            }
        }
        Ok(())
    }
}

/// Exercise everything a Phoenix Eternal task does, with a throwaway trader, so
/// the accounts behind each path end up in the local SVM and in the export.
/// Returns the mark price and the exchange metadata.
async fn phoenix_eternal(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    symbol: &str,
    phoenix_api: &str,
) -> Result<(f64, phoenix_rise::types::exchange::ExchangeResponse)> {
    let exchange = phoenix::fetch_exchange(phoenix_api).await?;
    let phoenix = Phoenix::new(exchange.clone(), surfnet.rpc_url());
    let market = phoenix.market(symbol)?;
    let warmup = Keypair::new();
    let trader = phoenix::onboard_trader(surfnet, &phoenix, &warmup, 1_000.0).await?;
    let hawkeye = phoenix.hawkeye(warmup.pubkey());
    let bbo = hawkeye.view_bbo(symbol).await?.value;
    hawkeye.view_margin(warmup.pubkey(), 0, 0).await?;
    hawkeye
        .view_margin_for_asset(warmup.pubkey(), 0, 0, market.asset_id)
        .await?;
    let mark_price_usd = phoenix.ticks_to_usd(market, bbo.mark_price_ticks);
    let bid = LimitOrderTicket::builder()
        .authority(warmup.pubkey())
        .trader_account(trader)
        .symbol(symbol)
        .side(Side::Bid)
        .price(mark_price_usd * 0.5)
        .num_base_lots(phoenix.base_units_to_lots(market, 1.0))
        .build()?;
    mtx_kit::send(
        rpc,
        &warmup,
        phoenix.tx().place_limit_order(bid).await?,
        "warmup bid",
    )?;
    let cancel = phoenix
        .tx()
        .build_cancel_all_orders(warmup.pubkey(), trader, symbol)?;
    mtx_kit::send(rpc, &warmup, cancel, "warmup cancel")?;

    // Scenario templates fetch accounts the trading path never reads (oracle
    // feeds, maker state). Move the market to where it already is so they load
    // without changing the price we export.
    chain::forward_surfnet_logs(surfnet);
    let perp_asset_map = phoenix.perp_asset_map()?;
    chain::register_scenario(
        rpc,
        "warmup market move",
        vec![
            Override {
                template_id: "phoenix-market-move",
                account: perp_asset_map,
                values: json!({ "symbol": symbol, "target_ticks": bbo.mark_price_ticks.to_string() }),
                slot: 2,
                label: "move to current mark".into(),
            },
            Override {
                template_id: "phoenix-market-uncross",
                account: perp_asset_map,
                values: json!({ "symbol": symbol }),
                slot: 2,
                label: "uncross".into(),
            },
        ],
    )?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    Ok((mark_price_usd, exchange))
}

/// Trade SOL/USDC on both spot venues with a throwaway trader. Returns the
/// Pyth SOL/USD price.
async fn spot_exercise(surfnet: &Surfnet, _rpc: &RpcClient) -> Result<f64> {
    let warmup = Keypair::new();
    crate::spot::fund(surfnet, &warmup, 10.0, 2_000.0, false)?;
    let price = crate::spot::exercise(surfnet.rpc_url(), &warmup).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    Ok(price)
}

/// Drops the two accounts every surfnet makes for itself from a random keypair,
/// its payer and its faucet: an offline start makes its own, and with them in
/// the export two builds of one recipe would differ.
fn without_surfnet_keys(
    mut accounts: std::collections::BTreeMap<String, Option<surfpool_sdk::AccountSnapshot>>,
    payer: &Pubkey,
) -> std::collections::BTreeMap<String, Option<surfpool_sdk::AccountSnapshot>> {
    const FAUCET_LAMPORTS: u64 = 100_000_000_000_000;
    accounts.retain(|address, account| {
        if *address == payer.to_string() {
            return false;
        }
        let Some(account) = account else { return true };
        let value = serde_json::to_value(account).unwrap_or_default();
        let lamports = value["lamports"]
            .as_u64()
            .or_else(|| value["lamports"].as_str().and_then(|l| l.parse().ok()))
            .unwrap_or(0);
        let system_owned = value["owner"].as_str() == Some("11111111111111111111111111111111");
        !(system_owned && lamports >= FAUCET_LAMPORTS)
    });
    accounts
}

/// Build the snapshot `recipe_path` describes and write it beside the recipe
/// (or to `opts.out_dir`).
pub async fn build(recipe_path: &Path, opts: BuildOptions) -> Result<Manifest> {
    let text = std::fs::read_to_string(recipe_path).map_err(io_error(recipe_path))?;
    let recipe = Recipe::parse(&text)?;
    recipe.validate()?;
    let dir = recipe_path.parent().unwrap_or(Path::new("."));
    let out_dir = opts.out_dir.unwrap_or_else(|| dir.to_path_buf());

    let (surfnet, source_manifest, fork_host) = match &recipe.fork.from_snapshot {
        Some(rel) => {
            let src = dir.join(rel);
            let (surfnet, manifest) = snapshot::start_snapshot(&src, Access::Open).await?;
            let name = src
                .canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_default();
            (surfnet, Some(manifest), format!("snapshot:{name}"))
        }
        None => {
            let url = opts.fork_rpc_url.clone();
            let surfnet = start_surfnet(|| {
                Surfnet::builder()
                    .remote_rpc_url(&url)
                    .block_production_mode(BlockProductionMode::Clock)
                    .slot_time_ms(SLOT_TIME_MS)
            })
            .await?;
            (surfnet, None, host_of(&opts.fork_rpc_url))
        }
    };
    chain::forward_surfnet_logs(&surfnet);
    let rpc = chain::rpc_client(&surfnet);

    // Declared programs and accounts first, so they load from the fork.
    let mut sources = Vec::new();
    for (role, list) in [("program", &recipe.programs), ("account", &recipe.accounts)] {
        for d in list {
            let address = parse_key(&d.address)?;
            let account = rpc.get_account(&address)?;
            if role == "program" && !account.executable {
                return Err(invalid(format!("{} ({address}) is not executable", d.name)));
            }
            sources.push(Source {
                role: d.kind.clone().unwrap_or_else(|| role.into()),
                name: d.name.clone(),
                address: address.to_string(),
            });
        }
    }

    let mut world = World {
        recipe: &recipe,
        dir,
        surfnet: &surfnet,
        rpc,
        mints: BTreeMap::new(),
        created: Vec::new(),
        exchange: None,
        price: None,
    };
    for action in &recipe.warmup {
        world.run(action).await?;
    }
    let World {
        rpc,
        created,
        exchange,
        price,
        ..
    } = world;
    let mark_price_usd =
        price.ok_or_else(|| invalid("the recipe's warm-up reports no price".into()))?;

    let (fork_slot, created_at_unix) = match &source_manifest {
        Some(m) => (m.fork_slot, m.created_at_unix),
        None => (
            rpc.get_slot()?,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default(),
        ),
    };
    let accounts = without_surfnet_keys(chain::export_snapshot(&rpc)?, &surfnet.payer().pubkey());
    for s in &sources {
        if !matches!(accounts.get(&s.address), Some(Some(_))) {
            return Err(invalid(format!(
                "{} ({}) is declared but the warm-up never loaded it",
                s.name, s.address
            )));
        }
    }
    std::fs::create_dir_all(&out_dir).map_err(io_error(&out_dir))?;
    write_gz(&out_dir.join(snapshot::SURFNET_GZ), &accounts)?;
    if let Some(exchange) = &exchange {
        write_json(&out_dir.join(snapshot::EXCHANGE_FILE), exchange)?;
    }
    let manifest = Manifest {
        created_at_unix,
        fork_host,
        fork_slot,
        accounts: accounts.len(),
        symbol: recipe.symbol.clone(),
        mark_price_usd,
        kind: recipe.kind.clone(),
        recipe_sha256: Some(recipe_hash(&text)),
        sources,
        simulated: !created.is_empty(),
        created,
    };
    write_json(&out_dir.join(MANIFEST_FILE), &manifest)?;
    info!(accounts = manifest.accounts, dir = %out_dir.display(), "snapshot written");
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
name = "t"
symbol = "SOL-USDC"
kind = "spot"

[[program]]
name = "phoenix-v1"
address = "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY"

[[warmup]]
action = "create-mint"
name = "FAKE"
decimals = 6
program = "token-2022"

[[warmup]]
action = "openbook-v2-market"
name = "FAKE/USDC"
base = "FAKE"
quote = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
base_lot_atoms = 1000
quote_lot_atoms = 10
price = { value = 12.5 }
ladder = { level_size = "10" }
"#;

    #[test]
    fn a_recipe_parses_and_hashes() {
        let recipe = Recipe::parse(SAMPLE).unwrap();
        recipe.validate().unwrap();
        assert_eq!(recipe.warmup.len(), 2);
        assert!(matches!(recipe.warmup[0], Action::CreateMint(_)));
        assert_eq!(recipe_hash(SAMPLE).len(), 64);
        assert_ne!(
            recipe_hash(SAMPLE),
            recipe_hash(&SAMPLE.replace("12.5", "13"))
        );
    }

    #[test]
    fn unknown_fields_and_actions_fail_loudly() {
        assert!(Recipe::parse("name = \"t\"\nsymbol = \"x\"\nbogus = 1").is_err());
        let bad = "name = \"t\"\nsymbol = \"x\"\n[[warmup]]\naction = \"teleport\"";
        assert!(Recipe::parse(bad).is_err());
    }

    #[test]
    fn a_duplicate_or_malformed_declaration_is_refused() {
        let twice = "name=\"t\"\nsymbol=\"x\"\n[[program]]\nname=\"a\"\naddress=\"11111111111111111111111111111111\"\n[[account]]\nname=\"a\"\naddress=\"11111111111111111111111111111111\"";
        assert!(Recipe::parse(twice).unwrap().validate().is_err());
        let bad = "name=\"t\"\nsymbol=\"x\"\n[[account]]\nname=\"a\"\naddress=\"nope\"";
        assert!(Recipe::parse(bad).unwrap().validate().is_err());
    }

    #[test]
    fn a_price_comes_from_a_value_or_a_calibration_key() {
        let dir = std::env::temp_dir();
        let value = PriceInput {
            value: Some(2.5),
            calibration: None,
            key: None,
        };
        assert_eq!(resolve_price(&value, &dir).unwrap(), Decimal::new(25, 1));
        assert_eq!(
            toml_number("[fair_value]\nprice = 84.5\n", "fair_value.price").unwrap(),
            84.5
        );
        assert!(toml_number("[a]\nb = \"x\"", "a.b").is_err());
        assert!(toml_number("[a]\nb = 1", "a.c").is_err());
        let both = PriceInput {
            value: Some(1.0),
            calibration: Some("x".into()),
            key: Some("k".into()),
        };
        assert!(resolve_price(&both, &dir).is_err());
        let negative = PriceInput {
            value: Some(-1.0),
            calibration: None,
            key: None,
        };
        assert!(resolve_price(&negative, &dir).is_err());
    }
}

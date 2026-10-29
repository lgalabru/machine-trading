//! What the viewer charts: one chart per venue and dimension.
//!
//! Every override lands on a chart. Overrides that move Phoenix prices go to
//! the Phoenix price chart, with one series per market, which mth samples
//! every slot. Every other template gets a chart for its venue and what it
//! changes (funding, fees, a lending rate…), so values in different units
//! never share an axis. mth doesn't sample those yet: their charts show when
//! each override lands, not a line.

use serde::Serialize;
use serde_json::{Value, json};
use surfpool_core::scenarios::TemplateRegistry;
use surfpool_types::{OverrideInstance, Scenario};

use crate::author::{self, Markets};

const MARKET_MOVE: &str = "phoenix-market-move";
const ORACLE_PRICE: &str = "phoenix-oracle-price";
const MARKET_UNCROSS: &str = "phoenix-market-uncross";
/// The id of the one chart mth samples.
pub const PHOENIX_PRICE: &str = "phoenix:price";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Chart {
    pub id: String,
    pub venue: String,
    pub title: String,
    /// Whether mth samples this chart's series every slot. A chart that
    /// isn't tracked only shows its overrides.
    pub tracked: bool,
    pub series: Vec<Series>,
}

/// One instrument on a chart.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Series {
    /// As a mandate names it: `phoenix:perp:SPCX`.
    pub key: String,
    pub symbol: String,
    pub start_usd: f64,
    /// The planned price, `[relative slot, usd]`, from slot 0.
    pub path: Vec<(u64, f64)>,
}

pub struct Layout {
    pub charts: Vec<Chart>,
    /// The enabled overrides in slot order, each with the `chart` it lands
    /// on and, on a tracked chart, its `series`.
    pub overrides: Vec<Value>,
}

/// Lay the scenario out on charts. `usd` converts a market's ticks to USD.
pub fn layout(
    scenario: &Scenario,
    registry: &TemplateRegistry,
    markets: &Markets,
    usd: impl Fn(&str, u64) -> Option<f64>,
) -> Layout {
    let mut enabled: Vec<&OverrideInstance> =
        scenario.overrides.iter().filter(|o| o.enabled).collect();
    enabled.sort_by_key(|o| o.scenario_relative_slot);

    // The price chart: every Phoenix market the scenario touches, in the
    // order it first does, or the default market when it touches none.
    let mut symbols: Vec<String> = Vec::new();
    for o in &enabled {
        if o.template_id.starts_with("phoenix-")
            && let Some(symbol) = author::symbol_of(o)
            && markets.get(&symbol).is_some()
            && !symbols.contains(&symbol)
        {
            symbols.push(symbol);
        }
    }
    if symbols.is_empty() {
        symbols.push(markets.default.clone());
    }
    let series = symbols
        .iter()
        .filter_map(|symbol| {
            let market = markets.get(symbol)?;
            Some(Series {
                key: key(symbol),
                symbol: symbol.clone(),
                start_usd: market.mark_usd,
                path: planned(&enabled, symbol, market.mark_ticks, market.mark_usd, &usd),
            })
        })
        .collect();
    let mut charts = vec![Chart {
        id: PHOENIX_PRICE.into(),
        venue: "Phoenix".into(),
        title: "Mark price".into(),
        tracked: true,
        series,
    }];

    let overrides = enabled
        .iter()
        .map(|o| {
            let (chart, series) = if [MARKET_MOVE, ORACLE_PRICE, MARKET_UNCROSS]
                .contains(&o.template_id.as_str())
            {
                (
                    PHOENIX_PRICE.to_string(),
                    author::symbol_of(o).map(|s| key(&s)),
                )
            } else {
                let template = registry.get(&o.template_id);
                let venue = venue(template.map_or(o.template_id.as_str(), |t| t.protocol.as_str()));
                let (dimension, title) = dimension(
                    &o.template_id,
                    template.map_or(o.template_id.as_str(), |t| t.name.as_str()),
                );
                let id = format!("{}:{dimension}", venue.to_lowercase().replace(' ', "-"));
                if !charts.iter().any(|c| c.id == id) {
                    charts.push(Chart {
                        id: id.clone(),
                        venue,
                        title,
                        tracked: false,
                        series: vec![],
                    });
                }
                (id, None)
            };
            json!({
                "slot": o.scenario_relative_slot,
                "label": o.label.clone().unwrap_or_else(|| o.template_id.clone()),
                "template": o.template_id,
                "values": o.values,
                "chart": chart,
                "series": series,
            })
        })
        .collect();
    Layout { charts, overrides }
}

pub fn key(symbol: &str) -> String {
    format!("phoenix:perp:{symbol}")
}

/// The price a market's moves aim for, step by step, from its mark.
fn planned(
    overrides: &[&OverrideInstance],
    symbol: &str,
    mark_ticks: u64,
    mark_usd: f64,
    usd: &impl Fn(&str, u64) -> Option<f64>,
) -> Vec<(u64, f64)> {
    let mut path = vec![(0, mark_usd)];
    let mut level = mark_ticks as f64;
    for o in overrides {
        if ![MARKET_MOVE, ORACLE_PRICE].contains(&o.template_id.as_str())
            || author::symbol_of(o).as_deref() != Some(symbol)
        {
            continue;
        }
        // An oracle move gives ticks or a change from the mark when it plays.
        let ticks = match author::target_ticks(o) {
            Some(ticks) => ticks as f64,
            None => match o
                .values
                .get("change_percent")
                .and_then(Value::as_str)
                .and_then(|p| p.trim().parse::<f64>().ok())
            {
                Some(pct) => level * (1.0 + pct / 100.0),
                None => continue,
            },
        };
        level = ticks;
        if let Some(price) = usd(symbol, ticks.round() as u64) {
            path.push((o.scenario_relative_slot, price));
        }
    }
    path
}

/// A venue's name from a template's protocol: "Phoenix Eternal" is Phoenix,
/// "kamino-scope" is Kamino Scope.
fn venue(protocol: &str) -> String {
    if protocol.to_lowercase().starts_with("phoenix") {
        return "Phoenix".into();
    }
    protocol
        .split(['-', ' ', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars
                .next()
                .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a template changes, so overrides in the same units share a chart:
/// `(id, title)`. Templates mth doesn't know get a chart of their own.
fn dimension(template_id: &str, name: &str) -> (String, String) {
    let (id, title) = match template_id {
        "phoenix-market-funding" => ("funding", "Funding"),
        "phoenix-market-fees" | "phoenix-trader-fees" => ("fees", "Fees"),
        "phoenix-market-risk-factors" | "phoenix-market-cancel-risk-factor" => {
            ("risk-factors", "Risk factors")
        }
        "phoenix-market-open-interest-cap" | "phoenix-market-max-liquidation-size" => {
            ("size-limits", "Size limits")
        }
        "kamino-reserve-interest-rate" => ("borrow-rate", "Borrow rate curve"),
        "kamino-reserve-state" => ("reserve-liquidity", "Reserve liquidity and rates"),
        "kamino-scope-price" | "kamino-scope-twap" | "pyth-price-feed-v2" => {
            ("oracle-price", "Oracle price")
        }
        _ => return (template_id.to_string(), name.to_string()),
    };
    (id.into(), title.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::author::Market;

    const PERP_ASSET_MAP: &str = "2nHGAaEw3D5dd4hVueaUNoygkQFmoeKqRQWnSPqSMFUC";

    fn markets() -> Markets {
        let market = |symbol: &str, mark_ticks, mark_usd| Market {
            symbol: symbol.into(),
            mark_ticks,
            mark_usd,
        };
        Markets {
            default: "SPCX".into(),
            markets: vec![market("SPCX", 14_000, 140.0), market("TSLA", 40_000, 400.0)],
            perp_asset_map: PERP_ASSET_MAP.into(),
            surfnet_port: 8899,
        }
    }

    fn scenario(overrides: Value) -> Scenario {
        let overrides: Vec<Value> = overrides
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let mut o = o.clone();
                o["id"] = json!(format!("o{i}"));
                o["enabled"] = json!(true);
                o["account"] = json!({ "pubkey": PERP_ASSET_MAP });
                o
            })
            .collect();
        serde_json::from_value(json!({
            "id": "s", "name": "s", "description": "", "tags": [], "overrides": overrides,
        }))
        .unwrap()
    }

    fn step(slot: u64, template: &str, values: Value) -> Value {
        json!({ "templateId": template, "values": values, "scenarioRelativeSlot": slot, "label": format!("{template} {slot}") })
    }

    /// Ticks are cents, for every market.
    fn usd(_: &str, ticks: u64) -> Option<f64> {
        Some(ticks as f64 / 100.0)
    }

    #[test]
    fn markets_on_one_venue_share_the_price_chart() {
        let s = scenario(json!([
            step(
                4,
                MARKET_MOVE,
                json!({ "symbol": "SPCX", "target_ticks": "13500" })
            ),
            step(
                4,
                MARKET_MOVE,
                json!({ "symbol": "TSLA", "target_ticks": "41000" })
            ),
            step(
                8,
                ORACLE_PRICE,
                json!({ "symbol": "TSLA", "change_percent": "2" })
            ),
            step(9, MARKET_UNCROSS, json!({ "symbol": "SPCX" })),
        ]));
        let layout = layout(&s, &TemplateRegistry::new(), &markets(), usd);
        assert_eq!(layout.charts.len(), 1);
        let chart = &layout.charts[0];
        assert!(chart.tracked);
        let symbols: Vec<&str> = chart.series.iter().map(|s| s.symbol.as_str()).collect();
        assert_eq!(symbols, ["SPCX", "TSLA"]);
        assert_eq!(chart.series[0].path, [(0, 140.0), (4, 135.0)]);
        // The oracle move compounds on the last level: 41,000 ticks + 2%.
        assert_eq!(chart.series[1].path, [(0, 400.0), (4, 410.0), (8, 418.2)]);
        assert!(layout.overrides.iter().all(|o| o["chart"] == PHOENIX_PRICE));
        assert_eq!(layout.overrides[1]["series"], "phoenix:perp:TSLA");
    }

    #[test]
    fn other_dimensions_and_venues_get_their_own_charts() {
        let s = scenario(json!([
            step(
                2,
                MARKET_MOVE,
                json!({ "symbol": "SPCX", "target_ticks": "13500" })
            ),
            step(3, "phoenix-market-funding", json!({ "symbol": "SPCX" })),
            step(4, "phoenix-market-fees", json!({ "symbol": "SPCX" })),
            step(5, "kamino-reserve-interest-rate", json!({})),
            step(6, "kamino-reserve-state", json!({})),
            step(7, "phoenix-market-funding", json!({ "symbol": "SPCX" })),
        ]));
        let layout = layout(&s, &TemplateRegistry::new(), &markets(), usd);
        let ids: Vec<&str> = layout.charts.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "phoenix:price",
                "phoenix:funding",
                "phoenix:fees",
                "kamino:borrow-rate",
                "kamino:reserve-liquidity"
            ]
        );
        assert!(
            layout.charts[1..]
                .iter()
                .all(|c| !c.tracked && c.series.is_empty())
        );
        assert_eq!(layout.charts[3].venue, "Kamino");
        assert_eq!(layout.overrides[5]["chart"], "phoenix:funding");
        assert_eq!(layout.overrides[1]["series"], Value::Null);
    }

    #[test]
    fn a_scenario_without_prices_still_charts_the_default_market() {
        let s = scenario(json!([step(3, "kamino-reserve-interest-rate", json!({}))]));
        let layout = layout(&s, &TemplateRegistry::new(), &markets(), usd);
        assert_eq!(layout.charts[0].series[0].symbol, "SPCX");
        assert_eq!(layout.charts[0].series[0].path, [(0, 140.0)]);
    }

    #[test]
    fn venues_read_as_names() {
        assert_eq!(venue("Phoenix Eternal"), "Phoenix");
        assert_eq!(venue("kamino-scope"), "Kamino Scope");
        assert_eq!(venue("Pyth"), "Pyth");
    }
}

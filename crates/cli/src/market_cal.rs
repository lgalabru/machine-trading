//! `mtx market-cal`: market-calibration files, the market model they drive,
//! and the realism checks (`docs/market-calibration.md`). Not `mtx
//! calibrate`, which writes scenarios' reference PnL.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Subcommand, ValueEnum};
use mtx_grader::{
    carry::CarryFamily,
    making::MakingFamily,
    market::{Calibration, MarketModel, ModelSpec, realism},
    scenario::{Scenario, ShockFamily},
    weekend::WeekendFamily,
};

use crate::Family;

#[derive(Subcommand)]
pub(crate) enum MarketCalCommand {
    /// Load and validate a calibration file; print its id, SHA-256, sources,
    /// limits and the numbers the model draws from.
    Show { calibration: PathBuf },
    /// Draw scenarios from a calibration's market model: each is the
    /// template scenario (mandate, flow, funding...) with its path and fair
    /// value drawn, and the draw recorded in `market_model`.
    Generate {
        #[arg(long)]
        calibration: PathBuf,
        /// A scenario file whose everything-but-the-path the drawn ones keep.
        #[arg(long)]
        template: PathBuf,
        #[command(flatten)]
        model: ModelArgs,
        #[arg(long)]
        out: PathBuf,
        /// Names are `<prefix>-<n>`.
        #[arg(long)]
        prefix: String,
        #[arg(long, default_value_t = 3)]
        count: usize,
        /// Keep hidden sets' seeds private.
        #[arg(long)]
        seed: u64,
    },
    /// Realism checks: stylized facts of sessions against the calibration's
    /// real windows. Prints a report; exits 2 if a check fails.
    Check {
        /// Scenario files (a file or a directory of them), `model` for the
        /// calibration's own market model, or a task family
        /// (flash-crash, market-making, funding-carry, weekend-gap) drawn
        /// with `--count` and `--seed`.
        target: String,
        #[arg(long)]
        calibration: PathBuf,
        #[command(flatten)]
        model: ModelArgs,
        /// Sessions to draw for `model` or a family.
        #[arg(long, default_value_t = 300)]
        count: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
}

#[derive(clap::Args)]
pub(crate) struct ModelArgs {
    /// The model spec, as TOML (`vol_mult`, `regime`, `start`,
    /// `waypoint_slots`, `jumps`, `gaps`). Without it, the defaults: draw
    /// everything from the calibration.
    #[arg(long)]
    spec: Option<PathBuf>,
    /// Scale volatility beyond the calibration (overrides the spec's).
    #[arg(long)]
    vol_mult: Option<f64>,
}

impl ModelArgs {
    fn spec(&self) -> Result<ModelSpec> {
        let mut spec = match &self.spec {
            Some(path) => {
                let text =
                    std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
                toml::from_str(&text).with_context(|| path.display().to_string())?
            }
            None => ModelSpec::default(),
        };
        if let Some(vol_mult) = self.vol_mult {
            spec.vol_mult = vol_mult;
        }
        Ok(spec)
    }
}

pub(crate) fn run(command: MarketCalCommand) -> Result<String> {
    match command {
        MarketCalCommand::Show { calibration } => {
            let cal = Calibration::load(&calibration)?;
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "calibration": cal.meta.calibration_id,
                "sha256": cal.sha256,
                "asset": cal.meta.asset,
                "symbol": cal.meta.symbol,
                "sources": cal.meta.sources,
                "limits": cal.meta.limits,
                "per_slot_sd": cal.per_slot_sd(mtx_grader::scenario::SLOT_SECONDS),
                "fair_value": cal.fair_value,
                "calendar": cal.calendar,
                "book": cal.book,
                "realism": cal.realism.is_some(),
            }))?)
        }
        MarketCalCommand::Generate {
            calibration,
            template,
            model,
            out,
            prefix,
            count,
            seed,
        } => {
            let cal = Calibration::load(&calibration)?;
            let template = Scenario::load(&template)?;
            let model = MarketModel::new(cal, model.spec()?).map_err(anyhow::Error::msg)?;
            let scenarios = model
                .sample_set(&template, &prefix, count, seed)
                .map_err(anyhow::Error::msg)?;
            std::fs::create_dir_all(&out).with_context(|| out.display().to_string())?;
            let mut written = Vec::new();
            for scenario in scenarios {
                let path = out.join(format!("{}.json", scenario.name));
                scenario.save(&path)?;
                written.push(path);
            }
            Ok(serde_json::to_string_pretty(&written)?)
        }
        MarketCalCommand::Check {
            target,
            calibration,
            model,
            count,
            seed,
        } => {
            let cal = Calibration::load(&calibration)?;
            let report = if target == "model" {
                realism::check_model(&cal, &model.spec()?, count, seed)
                    .map_err(anyhow::Error::msg)?
            } else if Path::new(&target).exists() {
                realism::check_scenarios(&cal, &target, &load_scenarios(Path::new(&target))?)
            } else if let Ok(family) = Family::from_str(&target, true) {
                let scenarios = match family {
                    Family::FlashCrash => ShockFamily::spcx().sample_set("check", count, seed),
                    Family::MarketMaking => MakingFamily::spcx().sample_set("check", count, seed),
                    Family::FundingCarry => CarryFamily::spcx().sample_set("check", count, seed),
                    Family::WeekendGap => WeekendFamily::spcx().sample_set("check", count, seed),
                };
                realism::check_scenarios(&cal, &target, &scenarios)
            } else {
                anyhow::bail!(
                    "{target} is not a scenario file or directory, `model`, or a task family"
                );
            };
            let output = serde_json::to_string_pretty(&report)?;
            if !report.passed {
                println!("{output}");
                std::process::exit(2);
            }
            Ok(output)
        }
    }
}

fn load_scenarios(path: &Path) -> Result<Vec<Scenario>> {
    if path.is_file() {
        return Ok(vec![Scenario::load(path)?]);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(path)
        .with_context(|| path.display().to_string())?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    files.retain(|f| f.extension().is_some_and(|e| e == "json"));
    files.sort();
    files.iter().map(|f| Ok(Scenario::load(f)?)).collect()
}

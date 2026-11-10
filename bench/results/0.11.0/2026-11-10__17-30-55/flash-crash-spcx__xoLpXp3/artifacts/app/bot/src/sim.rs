//! Offline simulator: runs the core over scenario JSON paths.
mod core;

use std::fs;

fn num(v: &serde_json::Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut p = core::Params::default();
    let mut files = Vec::new();
    let mut verbose = false;
    for a in &args {
        if let Some((k, v)) = a.strip_prefix("--").and_then(|s| s.split_once('=')) {
            let f: f64 = v.parse().unwrap();
            match k {
                "enter" => p.enter = f,
                "full_at" => p.full_at = f,
                "min_frac" => p.min_frac = f,
                "still_steps" => p.still_steps = f as usize,
                "still_step" => p.still_step = f,
                "still_net" => p.still_net = f,
                "stop" => p.stop = f,
                "tp_full" => p.tp_full = f,
                "tp_partial" => p.tp_partial = f,
                "max_hold" => p.max_hold = f as usize,
                "cooldown" => p.cooldown = f as usize,
                "anchor_win" => p.anchor_win = f as usize,
                "risk_move" => p.risk_move = f,
                "risk_share" => p.risk_share = f,
                "cost" => p.cost = f,
                "restop_frac" => p.restop_frac = f,
                "max_stops" => p.max_stops = f as usize,
                "mom_jump" => p.mom_jump = f,
                "mom_frac" => p.mom_frac = f,
                "max_size" => p.max_size = f,
                "mom_cont" => p.mom_cont = f,
                "mom_pause" => p.mom_pause = f as usize,
                "fade" => p.fade = f > 0.5,
                "mom_net" => p.mom_net = f,
                "anchor_zone" => p.anchor_zone = f,
                "half_below" => p.half_below = f,
                _ => panic!("unknown param {k}"),
            }
        } else if a == "-v" {
            verbose = true;
        } else {
            files.push(a.clone());
        }
    }
    let start_mark = 143.0;
    let cap = 70.0;
    let (mut tot_pnl, mut tot_bound, mut worst, mut wins, mut n) = (0.0, 0.0, f64::MAX, 0, 0);
    let mut max_dd: f64 = 0.0;
    let mut pnls = Vec::new();
    for f in &files {
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(f).unwrap()).unwrap();
        let dur = num(&v, "duration_slots") as usize;
        let path: Vec<(usize, f64)> = v["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pt| (num(pt, "slot") as usize, num(pt, "level_pct")))
            .collect();
        let mut tv = 0.0;
        let mut prev = 0.0;
        for &(_, l) in &path {
            tv += (l - prev).abs();
            prev = l;
        }
        let bound = cap * start_mark * tv / 100.0;
        let steps = dur / 2;
        let mut c = core::Core::new(p.clone(), cap, 1000.0);
        let mut pos = 0.0;
        let mut pi = 0;
        let mut level = 0.0;
        let mut dd: f64 = 0.0;
        for k in 0..steps {
            let slot = 2 * k + 1;
            while pi < path.len() && path[pi].0 <= slot {
                level = path[pi].1;
                pi += 1;
            }
            let mark = start_mark * (1.0 + level / 100.0);
            let target = c.step(k, mark, pos);
            if verbose && (target != pos || k % 20 == 0) {
                eprintln!("{f} k={k} slot={slot} mark={mark:.2} anchor={:.2} pos={pos} -> {target} eq={:.0}", c.anchor(), c.equity);
            }
            pos = target;
            dd = dd.max(c.drawdown());
        }
        // Final valuation at the last mark: account once more with no trade.
        let pnl = c.equity;
        tot_pnl += pnl;
        tot_bound += bound;
        worst = worst.min(pnl);
        if pnl > 0.0 {
            wins += 1;
        }
        n += 1;
        max_dd = max_dd.max(dd);
        pnls.push(pnl);
        if verbose {
            eprintln!("{f}: pnl={pnl:.0} bound={bound:.0} maxdd={dd:.0} {}", v["description"]);
        }
    }
    let mean = tot_pnl / n as f64;
    let var = pnls.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
    println!(
        "n={n} reward={:.4} pnl={tot_pnl:.0} bound={tot_bound:.0} mean={mean:.1} worst={worst:.0} wins={:.2} maxdd={max_dd:.0} sharpe={:.2}",
        (tot_pnl / tot_bound).clamp(0.0, 1.0),
        wins as f64 / n as f64,
        mean / var.sqrt()
    );
}

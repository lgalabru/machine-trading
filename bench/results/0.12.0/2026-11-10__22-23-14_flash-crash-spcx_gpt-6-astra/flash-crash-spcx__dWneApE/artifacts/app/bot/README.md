# SPCX shock bot

`run.sh` starts the checked native executable at `bin/bot`. It does not build,
download models, or require Python. The GCC unwinding library is bundled in
`lib/`; the other runtime libraries are Debian's C runtime and math library.
The bot writes logs to stderr and does not write to its installation directory.

## Trading and risk

The bot reads Phoenix's oracle, mark, position, and actual account equity each
turn through standard Solana RPC. It trades only SPCX, using the supplied keypair,
mandate, and turn clock through `mtx_kit::run`.

A compact, embedded tree model forecasts price changes from 44 causal features:
recent returns, local price ranges, impulse sizes and ages, prior reversals, and
session time remaining. Features use oracle prices so the bot does not mistake
its own mark impact for a new shock. The inventory planner compares long, flat, and short
positions while charging for turnover. This lets it follow a continuing shock,
fade a likely reversal, or retain/close inventory when forecasts are weak.

Risk controls operate independently of the forecasts:

- Use collateral plus unrealized PnL from Phoenix, including execution costs.
- Track the account's equity peak and reduce size as drawdown grows.
- Reserve 15% of the drawdown mandate; size against a 5% adverse one-turn move.
- Halt permanently above 70% of the drawdown limit or on a liquidation flag.
- Use SDK-capped, immediate orders in slices of 10 shares; leave no resting orders.
- Flatten when the additional equity read fails or prices are invalid.
- Avoid an unnecessary closing trade at the final turn, when positions are
  valued at the mark.

The strategy has no access to scenario paths at runtime. All forecasts are
compiled into the executable.

## Checked results

| Chain test | Total PnL | Fraction of hindsight bound | Maximum drawdown |
| --- | ---: | ---: | ---: |
| Six supplied practice sessions | $997.47 | 15.37% | $287.22 |
| First 72 generated sessions | $25,459.85 | 27.59% | $700.02 |
| Second 72 generated sessions | $30,485.73 | 29.73% | $499.87 |

The supplied practice reference earned $539.46: the bot earned 1.85 times that
amount. All tested sessions respected the market and position restrictions,
with no refused RPC calls, liquidation, or drawdown-limit violations. These are
sample results, not guarantees about other scenarios.

The 144 generated sessions earned $55,945.58 in total. Full reports and an
aggregate summary are in `validation/`. Eight release-mode tests check model/feature
parity, inventory planning, position caps, and the drawdown guard.

## Build and test

The development environment needs Rust and `/opt/mtx/sdk`; the grading environment
needs neither. Build installation is atomic, and `bin/bot.sha256` records the
installed executable's digest.

```bash
./build.sh
PATH=/usr/local/cargo/bin:$PATH cargo test --release
bash ./run.sh  # Requires the MTX_* session variables and turn socket.
```

## Model provenance

The model was trained only on generated scenarios from the documented public
family. `research/model-config.json` records the seeds, horizons, and model
parameters. `research/train_model.py` reproduces feature extraction and fitting;
`research/export_model.py` converts trained JSON trees to native Rust tables.
These development scripts are not invoked by the bot.

```bash
mtx scenario generate --out /tmp/train --prefix big --count 24000 --seed 100193722
mtx scenario generate --out /tmp/validate --prefix mv --count 2000 --seed 4516288
python3 research/train_model.py --train /tmp/train --validation /tmp/validate --out /tmp/models
python3 research/export_model.py /tmp/models /tmp/model.rs
```

Training requires the packages in `research/requirements-dev.txt`. Exporting a
new model does not replace the checked executable; it must be reviewed, tested,
and built separately before installation.

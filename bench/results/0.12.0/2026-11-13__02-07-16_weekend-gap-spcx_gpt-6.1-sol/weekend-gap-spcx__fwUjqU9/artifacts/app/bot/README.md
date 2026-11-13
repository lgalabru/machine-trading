# SPCX weekend strategy

`bot.wasm` is the ready-to-run `wasm32-wasip2` strategy component. Rebuild it with:

```sh
just build
```

The strategy reads confirmed SPCX transactions and decodes each taker's account, direction, quantity, and limit. It tracks individual accounts, repeated directional flow, trading cadence, and permanent versus decaying price impact. It reads Phoenix's spline midpoint so its own trades do not distort the price signal.

Four bundled tree ensembles forecast price changes at 32, 80, and 180 slots and at reopening. The policy uses the 80- and 180-slot forecasts, subtracts estimated trading costs, and maintains a no-trade region to avoid excessive turnover. Orders are sent in five-share slices. After reopening, the bot holds its remaining position through valuation.

Exposure stays below the 70-share mandate. The strategy reads its collateral and virtual quote balance to estimate actual equity, stops taking risk after a $600 drawdown, and reduces reopening exposure using the remaining drawdown allowance. It submits no resting orders and trades only the session's SPCX market.

The component has no runtime filesystem or network dependencies. Model weights are compiled into the WASM. All additional chain reads use the permitted read-only RPC interface.

## Optional model reproduction

The weights were trained on generated scenarios, independently of the practice and chain-validation sets. To recreate the training inputs:

```sh
mtx scenario generate --family weekend-gap --out trainlarge --prefix sample --count 2400 --seed 5931821
mtx scenario generate --family weekend-gap --out trainv2 --prefix v2 --count 7200 --seed 90461938
python3 -m pip install -r training/requirements.txt
python3 training/train.py trainlarge trainv2 --out src/models.bin
just build
```

Every fifth generated session is held out from fitting. The training tools are optional development utilities; the grader only needs the built component.

## Chain validation

The selected policy completed 72 generated sessions with $17,629.58 total PnL,
reward 0.22755, a $217.99 maximum drawdown, and no disqualifications. It was
profitable in 58 of 72 sessions. The six supplied practice sessions produced
$1,671.91 total PnL, reward 0.27556, and a $200.58 maximum drawdown.

Reports are in `validation/`. These are local validation results, not hidden
scenario results.

An additional 24 fresh generated sessions produced $4,851.22 PnL with no violations. Across all 96 generated sessions, reward was 0.21080 and maximum drawdown was $637.93.

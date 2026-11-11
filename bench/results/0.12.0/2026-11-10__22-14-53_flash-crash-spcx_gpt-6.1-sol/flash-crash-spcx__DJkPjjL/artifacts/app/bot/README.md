# SPCX shock bot

`bash run.sh` starts the prebuilt native executable. It needs only the session environment supplied by the grader. It does not build, download, or write files at runtime.

The strategy uses 90 causal price-history features, including impulse timing, recent ranges, and clusters of moves. Two embedded tree ensembles forecast oracle returns over three and six seconds. An 18-basis-point entry threshold and an 8-basis-point opposing exit threshold limit unnecessary trading. A third ensemble forecasts one-second returns to choose execution urgency. All models were trained on 32,000 independently generated scenarios.

The bot follows unfinished moves and fades moves whose expected return changes direction. Orders use the SDK's position-capped sheet and ten-share IOC slices. Moderate trades build toward a persistent target in 35-share steps when the short forecast allows it. Strong signals and risk exits trade immediately.

An independent guard reads actual Phoenix cross-margin equity, reserves room for an adverse jump and execution costs, reduces the position when drawdown room shrinks, and permanently flattens after drawdown reaches half the mandate's limit. There are no resting orders or other traded markets.

Development commands:

```sh
./build.sh
./bin/bot --self-test
cargo test --release
```

Models and the serialization test fixture are compiled into the executable. Python and a model runtime are not required. Builds check the embedded predictors and replace the executable atomically, so an interrupted build leaves the previous executable usable. Model provenance is recorded in `models.json`.

The final strategy passed 216 generated validation sessions: PnL $86,296.65, reward 0.296483, maximum drawdown $647.83, with zero mandate violations and zero turn overruns. Its previous version made $85,088.67 on the same sessions. Full reports are in `validation-72.json` and `validation-144.json`; the combined summary is in `validation.json`.

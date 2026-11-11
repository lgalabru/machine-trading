# Development research

These Python/C++ programs are offline research tools. The deployed Rust
executable has no Python, NumPy, sklearn, C++ runtime, or training-file dependency.

The final candidate's forecast models were trained on 20,000 scenarios generated
with seed 714680. Its switching-value ensemble uses fitted value iterations 10
and 24 from that set, and iteration 5 from another 20,000 scenarios generated
with seed 761942. The latter starts from the first set's learned value estimate.
Training assumes 10 basis points of execution cost per side. Forecasts use
exponentially discounted future increments; switching estimates use a clipped
Bellman update with transaction costs. All inference inputs use observed prices
and the supplied session clock.

Validation paths are separate from training. Seeds used for research were
8425971 (72), 572813 (72), 3961274 (500), 231899 (2,000), and 864592 (72).
No session names, scenario files, or future waypoints are read by the bot.

`large.py` and `second.py` train the models; `ensemble.py` compares switching
policies; `export_ensemble.py` emits the embedded model and prediction fixtures.
`make_stress.py` builds the constructed three-leg risk scenarios. The older
`sim.py`, `learn.py`, and `fitted.py` are retained as research history.

Research used Python 3.11, numpy 2.4.6 and scikit-learn 1.9.1. Build the development-only accelerator with `g++ -O3 -shared -fPIC research/eval.cpp -o /tmp/eval.so` before running the training scripts. The scripts expect
scenario directories and intermediate model files under `/tmp`, as named in
their source. `eval.cpp` is an accelerated approximation of price-path trading;
final candidates must be checked with `mtx grade`, since this approximation does
not reproduce every book fill or mark-price effect.

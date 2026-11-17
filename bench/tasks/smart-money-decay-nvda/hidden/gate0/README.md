# Gate 0

Two measurements, both on the engine.

**The ladder** (`just ladder smart-money-decay-nvda`, or `bench/checks/ladder/ladder.py`) grades every rung on fresh
72-session hidden sets: the reference, the informed bot (told the session's informed and decoy wallets through the
roster, `--tell-latent`), and the rungs that don't read wallets (`checks/bots/naive`). `gate0.py` reads the ladder's
output and prints the signal gate, the strength band and how often the order holds on bootstrapped sets:

```sh
python3 gate0.py <ladder-out>/smart-money-decay-nvda
```

**The fast model** (`model.py`) replays the same policies on sessions the engine recorded: graded with `nop` on
generated scenarios whose actors keep their tape (`"record": true`), with `--details` for the book step by step. Each
policy decides from what was public by then and trades at the step's touch plus the taker fee. It was used to tune
the family and the rungs between engine runs:

```sh
mtx scenario generate --family smart-money-decay --prefix rec --count 48 --seed 79 --out scen
# set actors.record = true in each, then
mtx grade --jobs 4 --snapshot snapshots/phoenix-perps-2026-11-13 --trusted --bot bots/starter \
  --report rec/report.json --details rec/details scen/*.json
python3 model.py rec/report.json rec/details/bot
```

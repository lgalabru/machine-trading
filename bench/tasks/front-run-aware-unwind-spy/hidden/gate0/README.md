# Gate 0

Every measurement is on the engine: `bench/checks/ladder/ladder.py` grades each rung of `checks/ladder.json` on a
hidden set of the `front-run-unwind` family, the informed bot told the session's latent state (`tell_latent`).
`gate0.py` pools a ladder run and prints the signal gate, the strength band and how often the order holds on
bootstrapped sets; `bytype.py` breaks each rung down by the predator the session drew.

```sh
python3 -I bench/checks/ladder/ladder.py front-run-aware-unwind-spy --mtx target/release/mtx \
  --snapshot snapshots/phoenix-perps-2026-11-13 --sets 1 --count 72 --jobs 4 --out <out>
python3 gate0.py <out>/front-run-aware-unwind-spy
python3 bytype.py <out>/front-run-aware-unwind-spy/set-1
```

## What the informed bot is told

The session's latent state (`front_run_latent`, `LATENT_FIELDS` family `front-run-unwind`): the front-runner's
wallet (roster `front-runner`, identity), which of the menu's predators it is (`predator`, identity), and that
type's fixed settings (`window_steps`, `threshold` in SPY, `share`, `persist_steps`, `hold_steps`, process facts of
the menu, the same every session that type is drawn). It is not told when or whether the predator trades ahead,
the path, or any draw of the session's own. It sells at the maker's bid as fast as keeps its own selling over the
window under 0.9 of the threshold, and leaves the rest to the broker.

## Results

Files beside this one: `probe2-15.txt` and `probe3-15.txt` (15 sessions on a public label, the design before
and after the rework), `gate0-v1-partial.txt` (72 sessions from a fresh secret, the first design, stopped after
six rungs) and `gate0-v2.txt` (72 sessions from a fresh secret, the rework).

**Gate 0 fails, and the card retires.** On the rework's 72-session set the informed bot makes $208 against the
passive rung's $466 (0.45x, needs 2x), the order holds on none of 2,000 bootstrapped sets, and the reference sits
at 0.16 of the informed bot (band 0.2-0.7; Sharpe 0.52). The randomised TWAP is closed (it loses $1,095, like the
plain TWAP): a predator that reads net selling over a window isn't fooled by random slices. Two blind policies
beat the informed bot instead:

- **Resting asks inside the maker.** The predator buys back what it sold ahead, and the best ask is the bot's;
  even buying back only under its own entry, its selling skews the maker down until it can.
- **Selling only at a good bid.** A seller that waits out a stolen bid paces itself under any trigger without
  reading anything, and waiting costs nothing: the broker prices the remainder at the scenario's own close, which
  no actor can move. That bounds told over the best fixed pace near e(1-1/R) (about 2) whatever the menu.

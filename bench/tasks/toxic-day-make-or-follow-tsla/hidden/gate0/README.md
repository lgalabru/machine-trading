# Gate 0: the engine's ladder on one hidden set

Version 2 (the one rework; version 1 failed on two dev sets, see DESIGN.md).
The prototypes ran as components with `mtx grade --draw toxic-day --count 72
--v2-sessions --trusted --components-only` on `phoenix-perps-2026-11-13`.
Every rung ran on the same 72 sessions from one fresh secret, after main's
#114 (actor tells) was merged. `summary.py` read the reports
(`gate0-run1.txt`). `flat` was not run: it never trades and scores 0.

| Rung | Total $ | Mean $ | Won |
| --- | --- | --- | --- |
| informed (told the fund's wallets) | -1,263 | -17.55 | 16/72 |
| reference | -15 | -0.21 | 8/72 |
| make | -434 | -6.02 | 17/72 |
| follow | -4,344 | -60.34 | 2/72 |
| switch | -1,200 | -16.66 | 9/72 |
| burst | -980 | -13.61 | 11/72 |

**FAIL.** The informed bot loses money, so nothing can be at least 2x the
best non-reading rung, the ladder's order holds in 0 of 2,000 bootstrapped
sets, the reference's band can't be measured (it is 0.06 of a negative
informed), and the reference's Sharpe is -0.02.

On dev3 (16 sessions, before #114) the same design had the informed bot at
+$31 a session (13/16 won) and the reference at +$9.8, a share of 0.27,
with the order holding in 0.84 of bootstraps. What changed is #114: the
informed fund now errs on 15% of its views (`hit_rate` 0.85), each wallet
with its own view. The told bot follows any net take by the fund's wallets,
and it now leans 9.4 times a session against 4.5 on dev3. The fund's flow
is no longer a clean signal, and following it pays a round trip for each
wrong view. This is my diagnosis from the lean counts; it has not been
tested in isolation.

## The rerun, with the bots adapted to #114

This is the coordinator's one rerun; adapting the bots to #114 doesn't
count as a design rework. `hit_rate` stays 0.85 (Michael's call). The bots
read flow over 150 slots (the longest horizon the fund trades on). A flow
counts only when at least two wallets are on its side and they outnumber
those on the other side. A lean never flips after entry. One fresh
72-session secret (`gate0-run2.txt`):

| Rung | Total $ | Mean $ | Won |
| --- | --- | --- | --- |
| informed | +1,513 | +21.02 | 48/72 |
| reference | +7.5 | +0.10 | 4/72 |
| make | -513 | -7.13 | 11/72 |
| follow | -1,666 | -23.14 | 12/72 |
| switch | -457 | -6.35 | 20/72 |
| burst | -231 | -3.21 | 20/72 |

- **Signal: pass.** Informed is positive, and every non-reading rung loses,
  so informed is at least 2x the best one in 1.000 of bootstraps.
- **Order: fail.** It holds in 0.476 of 2,000 bootstrapped sets (0.481 of
  5,000), against 0.85 needed.
- **Band: fail.** The reference is +$0.10 a session, 0.005 of informed
  (-0.03 weighted), with Sharpe 0.01.

The reference read a morning as toxic in 17 of 72 sessions. Over a 150-slot
window, a burst is only seen once it has mostly run, so its 60-slot markout
rarely clears the 10 bp bar. Even when the reference follows, it enters
late. The untold read no longer earns anything.

**Verdict: RETIRE**, per the coordinator's rule (the order fails).

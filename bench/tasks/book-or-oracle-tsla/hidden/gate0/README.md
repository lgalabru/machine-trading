# Gate 0: the calibration model for book-or-oracle-tsla

`model.py` simulates the card 3 family quickly. It is vectorised over sessions and steps every half second, one bot turn of 2 slots. It runs the ladder on shared draws:

- a TSLA-calibrated fair walk;
- Phoenix oracle and Pyth failures (stale, delayed, off_price);
- crowd pushes of the book with a half-life;
- noise, arbitrage and crowd takers;
- a competitor centred on the book that steps inside the bot down to a floor.

```sh
python3 model.py <seed> [sessions] ['<family overrides as JSON>']
```

`gate0-seed-{11,12,13}.txt` are the recorded runs on the card's family, 600 sessions each.

## Rungs

| Rung | What it does |
| --- | --- |
| `oracle-quoter`, `book-quoter`, `midpoint-blend` | The card's rungs |
| `cautious` | Quotes outside both the oracle and the book, so it never has to decide which is right |
| `cautious-fader` | `cautious`, plus a trade toward the oracle whenever the book is far from it |
| `oracle-taker` | Always trusts the oracle and fades the book toward it |
| `reference` | The card's reference: it scores staleness and the crowd, blends the two prices by precision, and widens when they disagree |
| `reference-lean` | The reference, plus a trade toward the oracle when it sees a crowd push |
| `told` | Knows which source is wrong in each episode, but nothing about the future |

## Result

**Gate 0 fails.** See the task's PR_NOTES for the report.

## Redesign 1: makers peg to the Phoenix oracle (`model_peg.py`)

In this redesign, the competitor and the Phoenix makers centre on the Phoenix oracle, as the house competitor does. A failure of the Phoenix oracle therefore moves the book with it, and only a crowd moves the book away from the oracle. The arbitrage takers and the crowd show on the tape as filled volume, against the bot or the competitor.

New rules that never decide which source is wrong, added to the gate:

| Rule | What it does |
| --- | --- |
| `cautious3` | Quotes outside the oracle, the book and Pyth |
| `book-fader`, `pyth-fader` | Trade toward the oracle, or toward Pyth, whenever they part |
| `flow-shy` | Pulls whichever side the tape is hitting |
| `*-fade-both` | `cautious3` or `flow-shy`, plus both fades |

The runs:

- `peg-seed-{11,12,13}.txt`: the card's odds, 600 sessions each.
- `peg-v2-seed-{21,22,23}.txt`: odds tilted toward oracle failures that hit both feeds (`{"hits":[0.2,0.2,0.6],"p":[0.5,0.9]}`), 600 sessions each.

Gate 0 still fails:

- **With the card's odds**, told is 1.25–1.27 times the best rule that never decides (`flow-shy-fade-both`). The ratio holds at 2× on 0% of bootstrapped sets.
- **With the tilted odds**, the ratio is 2.03–2.21, but it holds at 2× on only 55–81% of bootstrapped sets, short of the 85% needed. The reference is also at the edge of the strength band (0.21–0.24 of told).

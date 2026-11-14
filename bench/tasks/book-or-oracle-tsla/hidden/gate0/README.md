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

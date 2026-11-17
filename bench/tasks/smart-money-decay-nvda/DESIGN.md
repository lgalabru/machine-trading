# Design record: smart-money-decay-nvda

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. It is card B2 of the second Phoenix perps batch
(`TASK_CARDS_BATCH2.md`); the departures from the card are listed last.

## Thesis

A good bot profits here because it keeps a record of each wallet on the tape, follows the ones the price rewards, notices when a record turns, and tells flow that leads a move from flow that follows it.

- **Wins:** per-wallet markouts with a decaying record, following credible wallets net of chasers, exiting at the information horizon.
- **Loses:** following the market's net flow or the price's move pays costs on chasers and decoys; following the busiest wallet follows a chaser or a faded decoy; following a wallet on its lifetime record keeps following a decoy after it turns.
- **Regimes:** every session makes 3 to 6 sharp moves, up or down with equal odds; 1 to 3 informed, 1 to 3 decoy, 2 to 4 chaser and 3 to 6 noise wallets are seated in every session.
- **Mandate:** NVDA only, 30 NVDA position cap, $500 loss limit, 72 hidden sessions per grade; a position left at the end is closed at the oracle less 5 bp.

## Expert research

Reading who trades ahead of the price is the core of a prop desk's flow
book and of a market maker's toxicity filter. Informed traders move prices
before public information does (Kyle 1985; Glosten and Milgrom 1985); desks
score counterparties by markouts, the price move after each of their
trades, and stop trusting a counterparty whose markouts turn (Easley,
López de Prado and O'Hara 2012 on flow toxicity). The expert's method:

- **Credit by wallet, not by flow.** The market's net flow mixes informed
  traders with noise and with momentum traders who trade after the move;
  only a per-counterparty record separates them.
- **Decay and change points.** A counterparty's edge comes and goes. A
  lifetime average keeps trusting one whose information has gone stale;
  the expert weights recent markouts and drops a record that has turned.
- **Lead versus lag.** Flow that comes after the book has already moved
  its way is chasing, whatever its sign; flow that comes before is the
  only kind worth following.

## The central decision

Each step: follow the flow of the wallets the bot trusts (the cap their
way), stay out, or close. The defensible options and where each fails:

| Policy | Where it fails |
| --- | --- |
| Follow the market's net flow | chasers pile in after every move and faded decoys burst at random: it pays costs on both |
| Follow the price | the moves land within two slots: it buys after them and pays costs |
| Follow the busiest wallet | the busiest is a chaser or a decoy |
| Follow a wallet's lifetime record | a decoy's early record is as good as an informed wallet's, and it keeps the bot following after the decoy fades |
| Per-wallet decayed markouts (the reference) | it loses each informed wallet's first moves while it learns, and follows a faded decoy for a burst or two |

## Levers

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| Informed wallets | take the move's way from a horizon of 20 to 80 slots ahead, about one order every 15 slots each | follows them from the first take, out once they stop | each wallet's takes, marked out against the book: the tape (`getSignaturesForAddress` and `getTransaction` on the market) and the trader accounts' positions |
| Decoys | trade like the informed until 20 to 60% of the session, then in bursts on random sides with the same rhythm | drops them once their bursts stop paying | a record that turns: recent markouts against older ones |
| Chasers | take the way the book moved once it moved 8 to 20 bp over 40 slots | ignores them | their takes come after the book has moved their way |
| Noise | random sides | ignores it | markouts around zero |
| Costs | 3.5 bp taker fee and a half-spread of about 2 bp | trades a few times a session | its own fills |

## How often the informed are right

The informed wallets keep the actor library's default `hit_rate` of 0.85:
each wallet, on its own, reads 85 in 100 of its horizons right and trades
the rest the wrong way for the whole horizon (decoys do the same until they
fade). A desk's best counterparties are right most of the time, not every
time, and a perfect record would make one take a label. The instruction
states it as a process fact. It is why the informed bot and the reference
vote: each trusted wallet's net flow over 20 slots is a vote, they enter
when two agree (the one, when only one is trusted) and hold 80 slots with no
flips. On the engine's tapes a wrong horizon can't be seen in the price
before its burst is over (a median of 40 slots to move 8 bp against it,
against a 10-slot burst), so a stop doesn't help; a second wallet does.

## No tells

- Every role is seated in every session, from fresh wallets of the
  session's keyring, with one size distribution (lognormal around 0.15 NVDA)
  and one limit (50 bp past the book) for every role.
- A faded decoy keeps the informed traders' rhythm: bursts as long as the
  horizon, about as often as the moves come, at the same pace. Only the
  price after a burst tells it from an informed wallet.
- Up and down moves are equally likely, so a fixed direction or a blind
  rule nets about its costs.
- The informed bot is told the session's roster only (which wallets are
  informed, which are decoys): an identity fact (`LATENT_FIELDS`'s `*`
  roster). It isn't told the moves, when a decoy fades, the horizon or the
  edge.

## Detectability

In the fast model on the engine's recorded tapes, an informed wallet's
take marks out about +30 bp at 40 slots (sd about 20), noise's about 0
(sd about 14): two or three marked takes separate an informed wallet from
noise, and the first move of the session is where a reader pays to learn.

## Gate 0

On the engine's ladder (`hidden/gate0/`: README, `gate0.py`, `model.py`
and the recorded results), two 72-session hidden sets:

| Build | informed / best non-reading | reference, v2 weighted, of informed's | reference per-session Sharpe | order holds |
| --- | --- | --- | --- | --- |
| v1 (decoys trade all the time after fading) | every non-reading rung loses; informed +$8,387 | 0.34 | 0.84 | 1.000 |
| v1.1 (faded decoys burst; oracle valuation) | every non-reading rung loses; informed +$8,389 | 0.37 | 0.91 | 1.000 |

The v1.1 run is on the same two sets (the same secrets) as v1.

## Fairness

- The agent is told every range of the family, the roles and their pace,
  the mandate, how PnL is computed and scoring v2's rule in plain words.
  It is not told which wallet holds which role.
- Everything a good bot uses is on chain: every order is an ordinary
  Phoenix transaction, every trader's position is in its trader account,
  and the book and the oracle are in every step.
- The reference reads only the tape, the book and the oracle, and stays
  inside the mandate in every session.

## Generations

- **v1.** Gate 0 on the engine's ladder (two 72-session sets); the faded
  decoys then traded all the time.
- **v1.1 (this build).** Faded decoys trade in bursts with the informed's
  rhythm, so a faded decoy's activity is no tell; sessions are valued at the
  oracle (a close-out at the oracle less 5 bp); the igniter exploit and the
  probe rungs.

## Departures from the card

| Card | Built | Why |
| --- | --- | --- |
| Path: the calibration with a volatility level, which the informed see ahead | the calibration plus 3 to 6 sharp moves a session, 0.9 to 2.1 median jumps, landing in one or two slots | NVDA's calibrated path rarely moves 15 to 40 bp in a horizon, so the informed would hardly trade; with moves ramped over 4 to 20 slots momentum caught the rest of each move |
| Informed pace not given | 8 times the market's trade rate per wallet | at 24 times, informed bursts dominated the market's net flow and the best net-flow follower made about 0.4 of told |
| Informed bot told informed, decoys, chasers, horizon and edge | told the informed and decoy wallets only | the identity read is what the task tests; the rest the bot can read from the session |
| Exploit `igniter` must lose because of chasers' size, fees and refill | it loses by construction | chasers read the scenario's book path, not the bot's own pushes, so no push sets them off |

---
name: writing-guides
description: Writes and revises the interactive guides on the Machine Trading Protocol site (specs/guides/): instrument guides such as perp, strategy guides such as arbitrage or market making, and the glossary. Covers the teaching approach (one running example, one idea at a time, story before formula, ELI5, history notes only when real), the components (::: callouts, ```lab blocks backed by pure models, glossary hovers), checking every number a guide claims with `npm run check`, and looking at the rendered page. Use when the user asks to "write a guide", "add a guide pages", "explain an instrument", "add a strategy guide", "write the market making guide", "add a lab", "add a widget", "add a glossary term", "this guide is confusing", "simplify this guide", or to review or restructure anything under specs/guides/.
license: MIT
metadata:
  author: Solana Foundation
  version: 0.3.0
  repo: lgalabru/machine-trading
---

# Writing guides

A guide teaches one instrument or one strategy to someone who has
never traded it. A great guide gets a reader from "I have dollars and I'm
looking at SPCX" to understanding what can go wrong, without ever asking
them to hold an idea the page hasn't given them yet.

The specs are normative. A guide explains them and never adds a rule; where
the two disagree, the spec wins, and the guide says so.

## Where things are

| What | Where |
| --- | --- |
| A guide's text | `specs/guides/instruments/<kind>.md`, `specs/guides/strategies/<name>.md` |
| Its models (what the labs compute) | the `.js` file next to it |
| The glossary | `specs/guides/glossary.md` |
| The components, and their full API | `specs/site/guides/` (`mt.js`, `terms.js`, `mt.css`) and its `README.md` |
| The build | `specs/site/build.mjs`; `npm run build` in `specs/site` |
| The checker | `npm run check` in `specs/site` |
| The published pages | `https://lgalabru.github.io/machine-trading/guides/...` |
| The guides' index | `guides/index.html`, built from `GUIDE_FOLDERS` in `build.mjs`: a new folder of guides needs an entry there |

Read `specs/site/guides/README.md` for the callout kinds, the lab JSON and
the model contract before writing a lab. Copy the closest existing guide
rather than starting blank: `instruments/perp.md` for an instrument,
`strategies/arbitrage.md` for a strategy.

## The approach

### One running example, everywhere

Every guide uses the same reader and the same price: **you have $1,000,
and SPCX is at $100**, in every instrument guide, spot included. Where the
specs only name other markets (spot names SOL-USDC), say once that the SPCX
market is illustrative and the real ones work the same way. A strategy guide
may switch asset when its story needs it, as selling a treasury's 30,000 SOL
does: keep the $100, say whose holding it is, and say why it's that asset. Numbers carry across sections and across guides,
so a strategy guide can say "your 50 SPCX from the perp guide" and the
reader already knows them. Labs fix the shared numbers (`"fixed": { "entry":
100, "deposit": 1000 }`) rather than offering them as sliders.

Talk in the reader's terms: "SPCX moves to $110", "what your $1,000
becomes", "you hold 50 SPCX". Not "the underlying", "exit price" or "units"
until the guide has introduced them.

### One idea at a time

Order the guide so each idea arrives before anything that uses it. Before
writing, list the ideas in the order they appear and check each against
everything before it.

- **A lab only offers controls for ideas already explained.** Anything else
  is fixed in `"fixed"`, with a value the later section will explain. The
  first perp lab has no maintenance margin slider; it is fixed at 3% until
  the margin section explains it, and that section's lab adds the slider.
- **The same idea has the same value in every lab.** If maintenance margin
  is 3% in one lab, it is 3% in all of them, unless the lab is about
  changing it.
- **Prerequisites point back, not forward.** A forward reference ("the next
  section explains exactly where") is fine in a caption; a paragraph that
  needs a later section to make sense is in the wrong place.
- **Mechanics before rules.** The venue's mechanics (margin, liquidation,
  funding) come before the mandate's limits, and the mandate section comes
  before any lab that uses `max_position` or `max_drawdown_usd`.

### A ticket and a ledger

Every trade in a guide is shown the same two ways, with the same names:

- **The ticket**: every number of the trade, in one table, in the order the
  reader meets them, each with what it is and where it comes from. For a
  perp: SPCX price, margin (what you commit), leverage (the one number you
  choose), notional (margin × leverage), position (notional ÷ price).
- **The ledger**: what happens as the price moves, one row per price, with
  each column's formula in its header (`PnL: 50 × (SPCX − $100)`, `Equity:
  $1,000 + PnL`).

A later section that needs more reuses the ledger and **adds columns**
(notional now, maintenance, leverage now); it never renames a column or
starts a new table with different words for the same numbers. Labs use the
ticket's names too ("Margin: what you commit", "Position", "Equity"), and a
strategy with several legs gets one ticket column per leg.

Stop stacking complexity: if a section needs three new ideas, it is three
sections, or the ideas belong in an earlier ticket. When a reader says a
section "feels backward", look for a number used before the ticket defines
it, or the same number under two names.

### Story before formula

Every section opens with what happens to the reader's dollars, step by
step, and only then gives the general rule:

1. A short walkthrough: "You deposit the $1,000, and open a long."
2. The ticket, then the ledger: SPCX at $110, $100, $90, and the case that
   breaks.
3. Then the formulas, in a code block, in the ticket's words, with the
   example's numbers worked beside them.
4. Then the lab, to explore the cases in between.

When the reader asks "at which frequency?" or "how does X work?", the guide
was too abstract there: answer with a walkthrough and a table, and add them
to the guide.

### Plain words, then the term

The glance box and the ELI5 use no jargon at all. In the body, introduce a
term in bold the first time it means something to the reader ("your
deposit, your **margin**"), and define it in the glossary, not in a
glossary table on the page.

- `:::glance`: five or six bullets in plain words ("You put down: a deposit,
  called margin, not the full price").
- `:::eli5`: an everyday analogy that ends by mapping each part to its term
  ("The bet is the perp, the deposit is margin..."). A short ELI5 can also
  follow a hard idea ("Like a rental deposit").

### History when it's real

A `:::history` note explains where a name or an instrument comes from, when
there is a real story: why "long" and "short", Le Maire's 1609 short sale,
Shiller proposing perps in 1993, Long-Term Capital Management. Don't force
one into every section. Write only what you're confident of, hedge what you
aren't ("perhaps because"), and tell the user which anecdotes came from
memory so they can be checked before publishing.

### Labs that teach

- **Presets are the lesson plan.** Each preset is a named case whose
  `note` says what happens, so clicking through them in order teaches the
  section: the normal case, the case where it works, the case that breaks,
  and the surprise ("Funding liquidates you", "Over the cap").
- **Compare against doing nothing special.** Draw the perp against buying
  SPCX outright, the hedge against no hedge: a dashed `muted` series.
- **Put the reader's money on the y axis** ("What your $1,000 becomes"),
  and start it at zero when showing a cost, so a $54 cost on $1,000 doesn't
  look like a crash.
- **Split a total into its parts** in one panel (spot leg, short leg,
  funding, total dashed), and show the thing that breaks in its own panel
  (the short leg's margin against its maintenance margin).
- **Readouts say the consequence** in words: "liquidated: the margin is
  gone", "the short leg survived". Tones always come with an icon.

### Mandates

Every guide ends its mechanics with how a [mandate](specs/mandate.md)
applies: which limits bind and how to size under them, with the running
example ("50 SPCX under a $1,000 limit tolerates a $20 fall"). If v0 can't
express the strategy yet, say so and point at the mandate's "Changes in
later versions".

## The outline

Instruments and strategies use the same outline, in this order:

1. Title `Guide: <name>`, `Status: guide`, a two-paragraph intro naming the
   running example and the teaching-model caveat.
2. `:::glance`, then `:::eli5`.
3. **What it is**, then a first trade, step by step, with a table, then the
   formula, then the first lab.
4. One section per mechanism, each with its walkthrough and lab.
5. **Parameters**: a table of who sets each (mandate, venue profile, venue),
   or, for a strategy, what a bot decides before it trades.
6. **Risks**: a `:::group` of `::risk` callouts.
7. **When to use it, and when not to**: `::use` and `::avoid` in a `:::group`.
8. **Under a mandate**, then **Scenarios**: the capstone lab, playing a
   position through made-up markets under the mandate's limits.
9. **Check yourself**: `:::check` questions in the running example.
10. **Glossary**: one paragraph pointing to the glossary page.

## The glossary

Every term a guide uses lives in `specs/guides/glossary.md`, one `###`
entry per term under `##` topics. The first paragraph is what the hover
shows on every guide pages, so it must stand alone.

```
### Long

Also: longs, going long, long position
Not: as long as, how long
```

- `Also:` lists every form that means the term.
- `Not:` lists phrases that contain a form but mean something else. Common
  words need them: "as long as", "hedge fund", "spreads your dollars".
- After adding terms, build and scan the underlined words on every guide for
  false matches, and either add a `Not:` or drop the alias.

## Checking a guide

A guide makes numeric claims in three places: the prose, the tables, and
the preset notes. All three must match what the models compute.

0. **Probe the model before writing its claims.** Run its cases (a few
   lines of node calling the registered model) and write each preset's note
   from what it prints, not the other way round. If the cases don't each
   teach something different, change the model before the text.
1. **Run the checker.** In `specs/site`, `npm run check` prints every lab's
   readouts for its defaults and for each preset, under the preset's claim.
   Read them side by side and fix whichever is wrong. It exits non-zero on a
   model that throws or returns NaN.
2. **Recompute the prose.** Walkthrough tables and check-yourself answers
   aren't labs: compute them by hand or in `node -e`, with the same constants
   the labs fix.
3. **Build.** `npm run build` validates every lab (JSON, model registered,
   values in range, presets setting real controls) and fails on a broken
   one.
4. **Look at it.** Serve `specs/site/_site` (`python3 -m http.server --directory
   _site`) and screenshot with headless Chrome:

   ```sh
   C="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
   "$C" --headless=new --hide-scrollbars --window-size=1400,9000 \
     --virtual-time-budget=4000 --screenshot=/tmp/guide.png http://localhost:8799/guides/instruments/perp.html
   ```

   Chrome won't size a window below about 500px, so check phones through a
   page holding a 375px-wide `<iframe>` of the guide. Focus an `a.term` from
   that page's script to see a glossary hover. Look for colliding labels, a
   series hidden under another, thin bands, and controls that wrap badly.

## Publishing

Pushing to `main` publishes the site through the Specs workflow, after a
deploy of over ten minutes. A new push cancels the deploy in progress, so
batch changes and push once. If the deploy job times out while the build
job passed, re-run the failed job (`gh run rerun <id> --failed`). Commit with
Conventional Commits, `feat(guides): ...` or `docs(guides): ...`.

## Pitfalls seen so far

- A slider for an idea the page hasn't explained yet ("I'm lost in the first
  widget").
- Two labs using different values for the same parameter.
- Leverage above 1 ÷ maintenance margin: the position can't exist. Cap the
  slider below it, and have the model say so in its `note`.
- A preset note written before the model was final, and never re-checked.
- Prose that says money is "paid" continuously when it is only re-valued:
  say what actually moves and when.
- Unit slips in notes ("3,000 SPCX" for 300). Compute, don't estimate.
- The same number under different names in different tables ("You have",
  "What's left", "Equity"), or a ticket number (the leverage) left implicit
  in a table that depends on it.
- Ordinary words caught by the glossary hover ("the venue asks for" read as
  the ask price): drop the alias or add a `Not:`.

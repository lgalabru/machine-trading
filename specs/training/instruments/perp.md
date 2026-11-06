# Guide: perpetual futures (`perp`)

Status: guide

This guide explains perpetual futures to someone who has never traded one:
what a position is, what makes it gain and lose, what can wipe it out, and
how a [mandate](../../mandate.md) keeps an agent trading one inside limits.
The [instrument profile](../../instruments/perp.md) is the normative text;
when this guide and the profile disagree, the profile is right.

The figures are a teaching model with made-up prices. Margin covers one
position, and funding accrues every hour at a fixed rate. A
[venue profile](../../venues/), such as [Phoenix](../../venues/phoenix.md),
defines the real mark price, margin and funding.

:::glance
- **Kind:** `perp`, as in `phoenix:perp:SPCX`
- **You hold:** a signed quantity of the underlying. Positive is long, negative is short
- **You gain when:** the price moves your way
- **You pay or earn:** funding, for as long as you hold
- **Expiry:** none
- **Worst case:** liquidation, which takes the margin you posted
- **Mandate limits:** `max_position` per perp, `max_drawdown_usd` per account
:::

:::eli5
You and a friend bet on the price of apples. You say it will go up, your
friend says down. Nobody buys any apples: every time the price moves, the
loser pays the winner the difference. The bet never ends until one of you
walks away.

To make sure the loser can pay, each of you leaves a deposit with a referee.
If your losses eat almost all of your deposit, the referee ends your bet and
keeps it. And if far more people bet "up" than "down", the "up" side pays the
"down" side a small fee every hour, which keeps the bet's price close to the
real price of apples.

The bet is the perp, the deposit is margin, the referee ending your bet is
liquidation, and the hourly fee is funding.
:::

## What a perp is

A perpetual future is a contract that tracks an underlying's price, such as a
stock or a token, without anyone owning the underlying. Buying one (going
**long**) gains when the price rises. Selling one (going **short**) gains when
it falls. Shorting is as easy as buying, which is the main reason traders use
perps.

A perp has no expiry. A dated future converges to the underlying's price by
settling on its expiry date. A perp has no such date, so it is held close to
the underlying's price by **funding**: periodic payments between longs and
shorts, covered [below](#funding).

### Position and PnL

A position is a quantity of the underlying, in its base units: shares or
tokens, not dollars. It is positive when long and negative when short. Its
**notional** is the quantity times the price, and its **PnL** (profit and
loss) is the quantity times how far the price has moved since entry:

```
PnL = quantity × (mark price − entry price)
```

The **mark price** is the price the venue values positions at. It is usually
built from an index of other markets rather than the last trade on the venue,
so one odd trade can't liquidate anyone.

```lab
{
  "model": "perp.payoff",
  "title": "What you make or lose when you close",
  "lede": "Pick a side and a size. The line is your PnL at each price you could close at.",
  "controls": [
    { "id": "side", "label": "Side", "type": "choice", "options": [["long", "Long"], ["short", "Short"]], "value": "long" },
    { "id": "size", "label": "Position (units)", "min": 1, "max": 100, "step": 1, "value": 50, "format": "num" },
    { "id": "entry", "label": "Entry price", "min": 20, "max": 200, "step": 1, "value": 100, "format": "price" },
    { "id": "leverage", "label": "Leverage", "min": 1, "max": 25, "step": 0.5, "value": 5, "format": "x" },
    { "id": "maintenance", "label": "Maintenance margin (%)", "min": 0.5, "max": 4, "step": 0.5, "value": 3, "format": "num", "hint": "Equity below this share of notional is liquidated." }
  ],
  "presets": [
    { "label": "Careful long", "set": { "leverage": 2 }, "note": "2× leverage: the price has to fall by almost half before you're liquidated." },
    { "label": "Aggressive long", "set": { "leverage": 20 }, "note": "20× leverage: a fall of about 2% takes everything you posted." },
    { "label": "Short", "set": { "side": "short" }, "note": "A short is the mirror image: it gains as the price falls and is liquidated if it rises." }
  ],
  "caption": "PnL is a straight line: every dollar the price moves is worth one dollar per unit you hold. Leverage doesn't change the line, only how much margin backs it, and so where liquidation cuts it off."
}
```

## Margin, leverage and liquidation

You don't pay the notional to open a perp. You post **margin**, a deposit
that covers your losses. **Leverage** is notional divided by margin: at 5×,
$1,000 of margin backs a $5,000 position.

Leverage doesn't change how many dollars you make or lose per dollar the
price moves. It changes how many of those dollars are *yours*. At 5×, a 1%
move is 5% of your margin; at 20×, it's 20%.

The venue requires your equity (margin plus PnL) to stay above a
**maintenance margin**, a small share of the notional. When it falls below,
the venue **liquidates** you: it closes the position at the market, and what
is left of the margin covers the loss and the liquidation's fee. A
liquidation is final. If the price comes back an hour later, you are no
longer in the trade to recover.

```lab
{
  "model": "perp.leverage",
  "title": "How far the price can move before you're liquidated",
  "lede": "Move the leverage, and compare the distance to liquidation with how much this market moves on an ordinary day.",
  "controls": [
    { "id": "leverage", "label": "Your leverage", "min": 1, "max": 50, "step": 0.5, "value": 5, "format": "x" },
    { "id": "daily", "label": "An ordinary day's move (%)", "min": 0.5, "max": 15, "step": 0.5, "value": 3, "format": "num", "hint": "Volatile tokens move 5% or more on a normal day." },
    { "id": "maintenance", "label": "Maintenance margin (%)", "min": 0.5, "max": 5, "step": 0.25, "value": 1, "format": "num", "hint": "Leverage can't exceed 1 ÷ this." }
  ],
  "presets": [
    { "label": "Quiet stock", "set": { "daily": 1.5 }, "note": "A large stock moving about 1.5% a day." },
    { "label": "Volatile token", "set": { "daily": 8 }, "note": "A small token moving 8% a day: anything above about 11× is liquidated by an ordinary day." },
    { "label": "Max leverage", "set": { "leverage": 50 }, "note": "At 50× a move of about 1% liquidates you." }
  ],
  "caption": "The distance to liquidation shrinks roughly as one over the leverage. Where the curve dips into the shaded band, a normal day's move is enough to end the trade."
}
```

:::risk Leverage hides in plain sight
Doubling the leverage roughly halves the distance to liquidation, but price
moves grow with the square root of time. Halving the distance therefore cuts
the time a position typically survives by about four.
:::

## Funding

Since a perp never settles, something has to stop its price drifting away
from the underlying's. That something is **funding**: at a regular interval
the venue compares the perp's price with the underlying's, and moves money
between longs and shorts.

- When the perp trades *above* the underlying, funding is positive and
  **longs pay shorts**. Being long becomes more expensive, which pulls the
  price down.
- When the perp trades *below*, funding is negative and **shorts pay longs**.

Funding is paid on the notional, whatever the margin. It is small per
interval and adds up over time. It is also how perps are used without a view
on the price: a trader who is paid funding on a perp and holds the opposite
position elsewhere earns the funding with no price exposure.

```lab
{
  "model": "perp.funding",
  "title": "What funding costs or pays you over time",
  "lede": "The price never moves in this figure. Everything you see is funding.",
  "controls": [
    { "id": "side", "label": "Side", "type": "choice", "options": [["long", "Long"], ["short", "Short"]], "value": "long" },
    { "id": "size", "label": "Position (units)", "min": 1, "max": 100, "step": 1, "value": 50, "format": "num" },
    { "id": "price", "label": "Price", "min": 20, "max": 200, "step": 1, "value": 100, "format": "price" },
    { "id": "rate", "label": "Funding rate, per hour", "min": -3, "max": 3, "step": 0.05, "value": 0.15, "format": "bp", "hint": "Positive: longs pay shorts. 1 bp is 0.01%." },
    { "id": "days", "label": "Days held", "min": 1, "max": 90, "step": 1, "value": 30, "format": "d" }
  ],
  "presets": [
    { "label": "Calm market", "set": { "rate": 0.15 }, "note": "A small positive rate, about 13% a year: longs pay a modest carry." },
    { "label": "Crowded long", "set": { "rate": 2 }, "note": "Everyone is long. Holding a long now costs about 175% a year." },
    { "label": "Paid to short", "set": { "side": "short", "rate": 2 }, "note": "The other side of a crowded long: shorts collect what longs pay." },
    { "label": "Crowded short", "set": { "rate": -1.5 }, "note": "Negative funding: shorts pay longs." }
  ],
  "caption": "Funding grows in a straight line with time and notional. A rate that looks tiny per hour is large per year, so it matters most for positions held for days."
}
```

## Parameters

What a perp is, as the specs and a venue describe it:

| Parameter | Who sets it | What it means |
| --- | --- | --- |
| Instrument reference | The mandate | `<venue>:perp:<symbol>`, such as `phoenix:perp:SPCX`. |
| `max_position` | The mandate | The largest position, long or short, in base units. Open orders don't count until they fill. |
| `max_drawdown_usd` | The mandate | How far, in USD, the account's value may fall below its running peak. It covers the account, not one perp. |
| Base unit and lot size | The venue profile | What one unit of the position is, and the smallest step it trades in. |
| Mark price | The venue | The price positions are valued and liquidated at. |
| Funding rate and interval | The venue | How much longs and shorts pay each other, and how often. |
| Initial and maintenance margin | The venue | The margin needed to open a position, and the margin below which it is liquidated. Together they cap leverage. |
| Fees | The venue | What each trade costs, usually less for orders that rest on the book than for orders that take from it. |

The mandate's two limits are the ones an agent is held to. The venue's are
the ones the market holds everyone to.

## Risks

:::group
::risk Price risk
A perp gains and loses one dollar per unit for every dollar the price moves.
A short has no ceiling on its loss: the price can rise without limit.
::
::risk Liquidation
With leverage, a move well inside a normal day's range can close the position
and take its margin. The move that caused it may reverse right after.
::
::risk Funding drag
A position held against crowded funding pays every interval. Over weeks the
carry can exceed what the trade was meant to earn.
::
::risk Gaps and thin books
Prices can jump past a stop or a liquidation level, as in a flash crash. A
large order in a thin book moves the price against itself as it fills.
::
::risk Mark and index
Positions are valued at the venue's mark, not the price you see elsewhere. A
mark that lags or diverges from other markets can liquidate a position the
other markets say is safe.
::
::risk Venue risk
The venue holds the margin. Its software, its oracles and its solvency are
part of the trade.
::
:::

## When to use it, and when not to

:::group
::use
- You want to be **short** as easily as long.
- You want exposure **without holding** the underlying, or with less capital
  up front.
- You want to **hedge** a holding: a short perp offsets a spot position's
  price risk.
- You want to **earn funding**, with an offsetting position elsewhere.
- You trade **actively**, in and out, where funding over hours is negligible.
::
::avoid
- You plan to hold for **months**, and funding runs against you.
- You can't **watch** the position, or have an agent watch it, through
  volatile hours.
- The market is **thin**, so the size you need moves the price.
- You'd use **high leverage** on a market that moves a lot in an ordinary day.
- You need the **underlying itself**, to vote, stake or transfer.
::
:::

## Scenarios

Each scenario plays 48 hours of a made-up market against one perp position,
under a mandate. The three panels share a time axis: hover one to read all
three at that hour. The top panel is the market, the middle what the position
made, and the bottom how far it fell from its best, against the mandate's
drawdown limit.

```lab
{
  "model": "perp.scenario",
  "title": "A position through a market, under a mandate",
  "lede": "Choose a market, then size the position and its limits.",
  "controls": [
    { "id": "path", "label": "Market", "type": "choice", "options": [["rally", "Rally"], ["chop", "Chop"], ["bleed", "Slow bleed"], ["flash-crash", "Flash crash"], ["squeeze", "Short squeeze"]], "value": "flash-crash" },
    { "id": "side", "label": "Side", "type": "choice", "options": [["long", "Long"], ["short", "Short"]], "value": "long" },
    { "id": "size", "label": "Position (units)", "min": 0, "max": 120, "step": 1, "value": 50, "format": "num" },
    { "id": "leverage", "label": "Leverage", "min": 1, "max": 20, "step": 0.5, "value": 3, "format": "x" },
    { "id": "maintenance", "label": "Maintenance margin (%)", "min": 0.5, "max": 4, "step": 0.5, "value": 3, "format": "num" },
    { "id": "funding", "label": "Funding rate, per hour", "min": -3, "max": 3, "step": 0.05, "value": 0.15, "format": "bp" },
    { "id": "cap", "label": "Mandate: max_position", "min": 10, "max": 120, "step": 1, "value": 70, "format": "num" },
    { "id": "limit", "label": "Mandate: max_drawdown_usd", "min": 100, "max": 3000, "step": 50, "value": 1000, "format": "usd0" }
  ],
  "presets": [
    { "label": "Long into a flash crash", "set": { "path": "flash-crash", "side": "long", "size": 50, "leverage": 3 }, "note": "A 22% fall in an hour. The position survives the venue but breaks the drawdown limit, which no recovery undoes." },
    { "label": "Same, at 5×", "set": { "path": "flash-crash", "side": "long", "size": 40, "leverage": 5 }, "note": "At 5× the venue liquidates the position at the bottom, and it misses the rebound." },
    { "label": "Sized for the limit", "set": { "path": "flash-crash", "side": "long", "size": 40, "leverage": 3 }, "note": "40 units: the same crash costs less than the drawdown limit, if only just, and the mandate holds." },
    { "label": "Short into a squeeze", "set": { "path": "squeeze", "side": "short", "size": 40, "leverage": 8 }, "note": "A short squeeze: the price jumps 20% in three hours and the short is liquidated." },
    { "label": "Paying to wait", "set": { "path": "chop", "side": "long", "size": 60, "funding": 2.5 }, "note": "A market going nowhere, with crowded funding: the position bleeds funding every hour." },
    { "label": "Over the cap", "set": { "path": "rally", "size": 90 }, "note": "A winning trade that breaks the mandate anyway: 90 units against a cap of 70." }
  ],
  "caption": "The drawdown limit counts from the running peak, not from entry: a position that first gains and then gives it back can break it without ever being down overall. Sizing so that `max_drawdown_usd ÷ position` is larger than the moves you expect keeps the mandate."
}
```

## Trading a perp under a mandate

The [mandate](../../mandate.md) bounds an agent with two numbers for a perp,
and the agent is responsible for both:

- **`max_position` caps the filled position**, in base units. Resting orders
  don't count until they fill, so an agent that must stay under the cap sizes
  its resting orders so that fills can't take it over.
- **`max_drawdown_usd` caps the fall from the peak**, in USD, for the whole
  account. Dividing it by the position gives the price move, from the peak,
  that breaks the limit. A position of 40 under a $1,000 limit tolerates a
  $25 move; a position of 70 tolerates about $14.

The venue's liquidation is a separate, harder floor. An agent that keeps
the mandate's drawdown limit normally stays far from liquidation, but leverage
decides how far: the mandate limits the loss in dollars, the venue limits it
as a share of the margin.

:::example The benchmark's `phoenix-flash-crash` task
A bot trades `phoenix:perp:SPCX` under a cap of 70 and a $1,000 drawdown
limit, through replayed sessions in which the price may crash. Holding the
full cap into a crash of more than about $14 breaks the limit, so a bot has
to size for the crash it can't predict, or cut quickly when it starts.
:::

## Check yourself

:::check You're long 10 units from $100 at 5×. The price falls to $95. What is your PnL, and what share of your margin is it?
−$50. The notional was $1,000 and the margin $200, so −$50 is 25% of the
margin: 5× the 5% move.
:::

:::check Funding is +1 bp an hour and you're short $10,000 of notional. Do you pay or receive, and how much a day?
You receive. Positive funding means longs pay shorts: 0.01% × $10,000 × 24 =
$24 a day.
:::

:::check Your mandate's drawdown limit is $1,000 and your position is 50 units. How far can the price move against you from its peak before you break it?
$20 a unit: $1,000 ÷ 50. Funding paid along the way uses up some of that room.
:::

:::check Why can a winning trade still break the mandate?
Because the position cap is independent of PnL: a position over
`max_position` breaks the mandate whether or not it is making money. And the
drawdown limit counts from the peak, so giving back gains counts too.
:::

## Glossary

| Term | Meaning |
| --- | --- |
| Long, short | A positive position gains when the price rises; a negative one when it falls. |
| Base units | What the position is counted in: shares or tokens, not dollars. |
| Notional | The position's size in dollars: quantity times price. |
| Mark price | The price the venue values and liquidates positions at. |
| Index price | The underlying's price, from other markets, that the mark and funding follow. |
| Margin | The deposit that backs a position. |
| Leverage | Notional divided by margin. |
| Maintenance margin | The equity below which the venue liquidates the position. |
| Liquidation | The venue closing a position whose equity fell below maintenance margin. |
| Funding | Periodic payments between longs and shorts that keep the perp near the index. |
| Drawdown | How far the account's value is below its running peak. |

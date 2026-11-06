# Guide: perpetual futures (`perp`)

Status: guide

This guide explains perpetual futures to someone who has never traded one:
what a position is, what makes it gain and lose, what can wipe it out, and
how a [mandate](../../mandate.md) keeps an agent trading one inside limits.
The [instrument profile](../../instruments/perp.md) is the normative text;
when this guide and the profile disagree, the profile is right.

It builds one idea at a time, on a single example: you have $1,000, and SPCX
trades at $100. The figures use made-up prices and a simplified venue; a
[venue profile](../../venues/), such as [Phoenix](../../venues/phoenix.md),
defines the real rules.

:::glance
- **You bet on:** a price going up (long) or down (short)
- **You put down:** a deposit, called margin, not the full price
- **You gain or lose:** $1 per unit for every $1 the price moves
- **It never expires:** you hold it as long as you like
- **While you hold:** you pay or earn a small fee, called funding
- **Worst case:** the venue closes your bet and keeps the deposit
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

A perp never expires: you hold it for as long as you like. What keeps its
price close to the underlying's is a payment between longs and shorts called
funding, which comes [later](#funding), once the basics are in place.

:::history Where perps come from
The economist Robert Shiller proposed perpetual futures in 1993, as a way
to trade things like house prices that have no market to settle against.
They took off in crypto instead: BitMEX launched a perpetual swap on bitcoin
in 2016, and perps are now the most traded crypto derivative.
:::

### Your first trade

You have **$1,000**, and you think SPCX, trading at **$100**, is going up.
Without a perp, your $1,000 buys 10 SPCX. With a perp, you open a long
position on a venue instead, and the trade comes down to five numbers, in
the order you meet them:

| Number | Your trade | What it is |
| --- | --- | --- |
| **SPCX price** | $100 | What SPCX trades at when you open. |
| **Margin** | $1,000 | What you commit: you deposit it on the venue. It's the most you can lose. |
| **Leverage** | 5× | The one number you choose: how many times your margin the bet is. |
| **Notional** | $5,000 | The size of your bet in dollars: margin × leverage. You never pay it. |
| **Position** | 50 SPCX | The notional in SPCX: $5,000 ÷ $100. Long is +50; short would be −50. |

You don't own any SPCX. You hold a contract that gains $50, $1 for each of
your 50 SPCX, every time SPCX rises $1, and loses $50 every time it falls $1.
Buying 10 SPCX outright is the same ticket at 1×: a $1,000 notional, 10 SPCX.

### As SPCX moves

Two more numbers follow from the ticket as SPCX moves. Your **PnL** (profit
and loss) is your position times how far SPCX has moved. Your **equity** is
your margin plus your PnL: what's left, and what you'd walk away with if you
closed now.

| SPCX | PnL: 50 × (SPCX − $100) | Equity: $1,000 + PnL |
| --- | --- | --- |
| $110 | +$500 | $1,500 |
| $100 | $0 | $1,000 |
| $90 | −$500 | $500 |
| ≈ $82.47 | ≈ −$876 | ≈ $124, and the venue closes your position |

The last row is the catch: the venue closes the position before your equity
reaches zero. That is **liquidation**, and the [next
section](#leverage-and-liquidation) explains exactly where it happens. Going
**short** is the mirror image: short 50 SPCX gains $50 for every $1 SPCX
falls.

The ticket and the ledger, in four lines:

```
notional = margin × leverage                 $1,000 × 5    = $5,000
position = notional ÷ SPCX at entry          $5,000 ÷ $100 = 50 SPCX
PnL      = position × (SPCX now − SPCX at entry)
equity   = margin + PnL
```

No money changes hands while SPCX moves: the venue re-values your position
each time its **mark price** for SPCX updates, several times a second on an
on-chain venue, and your equity moves with it. The PnL becomes real dollars
when you close. The mark price is built from other markets' prices rather
than the venue's last trade, so one odd trade can't liquidate anyone.

```lab
{
  "model": "perp.payoff",
  "title": "What your dollars become",
  "lede": "The ticket from above: set your margin and leverage, then drag where SPCX goes. The blue line is your equity with the perp, the grey one what your margin would be worth if you had bought SPCX with it.",
  "controls": [
    { "id": "side", "label": "You think SPCX will", "type": "choice", "options": [["long", "Go up (long)"], ["short", "Go down (short)"]], "value": "long" },
    { "id": "deposit", "label": "Margin: what you commit", "min": 100, "max": 10000, "step": 100, "value": 1000, "format": "usd0" },
    { "id": "leverage", "label": "Leverage", "min": 1, "max": 25, "step": 0.5, "value": 5, "format": "x", "hint": "Notional ÷ margin." },
    { "id": "exit", "label": "SPCX moves to", "min": 50, "max": 150, "step": 0.5, "value": 110, "format": "price" }
  ],
  "fixed": { "maintenance": 3, "entry": 100 },
  "presets": [
    { "label": "Like buying the shares", "set": { "leverage": 1 }, "note": "1×: the perp line sits on the grey one. Long 10 SPCX is the same bet as buying 10 SPCX, and only SPCX going to zero wipes it out." },
    { "label": "5×, SPCX up 10%", "set": { "leverage": 5, "exit": 110 }, "note": "A 10% rise turns $1,000 into $1,500: five times what buying the shares makes." },
    { "label": "5×, SPCX down 10%", "set": { "leverage": 5, "exit": 90 }, "note": "The same 10% the other way costs $500, half your money." },
    { "label": "20×, a small dip", "set": { "leverage": 20, "exit": 97.5 }, "note": "At 20×, a 2.5% dip is enough to liquidate you." },
    { "label": "Short, SPCX falls", "set": { "side": "short", "leverage": 5, "exit": 90 }, "note": "Short 50 SPCX: the 10% fall that cost the long $500 makes the short $500." }
  ],
  "caption": "Each $1 SPCX moves is worth $1 per unit you hold. Leverage sets how many units your dollars carry, so it steepens the line, and it moves the liquidation point closer: past it, the line drops to zero. The [next section](#leverage-and-liquidation) explains exactly where that point is."
}
```

## Leverage and liquidation

Leverage is the one number on the ticket you choose. With the same $1,000 of
margin, it sets everything else:

| Leverage | Margin | Notional | Position | Each $1 SPCX moves | SPCX moves 1% |
| --- | --- | --- | --- | --- | --- |
| 1× | $1,000 | $1,000 | 10 SPCX | $10 | 1% of your margin |
| 5× | $1,000 | $5,000 | 50 SPCX | $50 | 5% of your margin |
| 20× | $1,000 | $20,000 | 200 SPCX | $200 | 20% of your margin |

Leverage multiplies the percentage you gain or lose on your margin. It
works in both directions, and the losing direction has a floor.

### Liquidation

The venue doesn't let your equity fall to zero. It requires it to stay
above the **maintenance margin**: a share of what your position is worth
now, 3% on this venue. Here is the ledger from your first trade again, at
5×, with the columns the venue watches:

| SPCX | PnL | Equity | Notional now (50 × SPCX) | Maintenance (3% of notional) | Leverage now (notional ÷ equity) |
| --- | --- | --- | --- | --- | --- |
| $100 | $0 | $1,000 | $5,000 | $150 | 5× |
| $90 | −$500 | $500 | $4,500 | $135 | 9× |
| $85 | −$750 | $250 | $4,250 | $127.50 | 17× |
| ≈ $82.47 | ≈ −$876 | ≈ $124 | ≈ $4,124 | ≈ $124 | 33×: **liquidated** |

Liquidation happens where equity meets the maintenance margin:

```
1,000 + 50 × (P − 100) = 0.03 × 50 × P
1,000 + 50 P − 5,000   = 1.5 P
48.5 P                 = 4,000
P                      = 82.47
```

The last column shows the same thing from the other side. The position
stays 50 SPCX while your equity shrinks, so your leverage climbs as you
lose: 5×, 9×, 17×. The venue liquidates you when it reaches 1 ÷ 3%, about
33×.

When it does, the venue sells your 50 SPCX into the market, at whatever the
price is by then. It doesn't wait for zero because selling takes time and
costs money: the $124 cushion pays for that and for the venue's liquidation
fee, so your loss never lands on the venue or other traders. Whatever is left
may come back to you, depending on the venue; this guide counts it as lost.
A liquidation is final: if SPCX is back at $100 an hour later, you are no
longer in the trade.

To open a position, the venue asks for more than the maintenance margin: the
**initial margin**. If it is 10%, a $5,000 notional needs at least $500 of
margin, so 10× is the most you can open with. The gap between the two is your
room to lose. Leverage above 1 ÷ maintenance (33× here) is impossible: the
position would be liquidated the moment it opened.

:::eli5 Like a rental deposit
A landlord holds your deposit while you live there. If the damage you do
nearly uses it up, they end the lease before it's all gone, keeping enough
to pay for the cleanup.
:::

```lab
{
  "model": "perp.leverage",
  "title": "Where SPCX has to go to liquidate you",
  "lede": "Your $1,000 again, SPCX at $100. Move the leverage and watch the liquidation prices close in on today's price. The shaded band is how far SPCX moves on an ordinary day.",
  "controls": [
    { "id": "leverage", "label": "Your leverage", "min": 1, "max": 30, "step": 0.5, "value": 5, "format": "x" },
    { "id": "daily", "label": "SPCX on an ordinary day (± %)", "min": 0.5, "max": 15, "step": 0.5, "value": 3, "format": "num", "hint": "A big stock moves 1–2% a day; a small token 5% or more." },
    { "id": "maintenance", "label": "Maintenance margin (%)", "min": 1, "max": 5, "step": 0.5, "value": 3, "format": "num", "hint": "The floor from the table above. Leverage can't exceed 1 ÷ this." }
  ],
  "fixed": { "entry": 100, "deposit": 1000 },
  "presets": [
    { "label": "Calm SPCX", "set": { "daily": 1.5 }, "note": "SPCX moving about 1.5% a day, like a large stock: an ordinary day only liquidates you past about 22×." },
    { "label": "Wild SPCX", "set": { "daily": 8 }, "note": "SPCX moving 8% a day: from about 9.5× up, an ordinary day liquidates you." },
    { "label": "30×", "set": { "leverage": 30 }, "note": "At 30×, $1,000 holds 300 SPCX, and SPCX falling to about $99.66 ends the trade: a third of a percent." }
  ],
  "caption": "The liquidation price moves toward today's price roughly as one over the leverage. Once a line enters the band, a normal day's move is enough to end the trade."
}
```

:::risk Leverage hides in plain sight
Doubling the leverage roughly halves the distance to liquidation, but price
moves grow with the square root of time. Halving the distance therefore cuts
the time a position typically survives by about four.
:::

## Funding

A **dated future**, the older kind of future, has an expiry date on which it
settles at the underlying's price, so its price has to meet the
underlying's by then. A perp never settles, so something else has to stop
its price drifting away from the underlying's. That something is **funding**: at a regular interval
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
  "title": "What funding does to your $1,000",
  "lede": "SPCX stays at $100 the whole time in this figure: everything that happens to your dollars is funding.",
  "controls": [
    { "id": "side", "label": "You are", "type": "choice", "options": [["long", "Long"], ["short", "Short"]], "value": "long" },
    { "id": "leverage", "label": "Leverage", "min": 1, "max": 20, "step": 0.5, "value": 5, "format": "x", "hint": "Funding is paid on the position, not on your dollars." },
    { "id": "rate", "label": "Funding rate, per hour", "min": -3, "max": 3, "step": 0.05, "value": 0.15, "format": "bp", "hint": "Positive: longs pay shorts. 1 bp is 0.01%." },
    { "id": "days", "label": "Days held", "min": 1, "max": 90, "step": 1, "value": 30, "format": "d" }
  ],
  "fixed": { "maintenance": 3, "entry": 100, "deposit": 1000 },
  "presets": [
    { "label": "Calm market", "set": { "rate": 0.15 }, "note": "About 13% a year on the position. At 5× that is about 65% a year on your $1,000: $54 over a month." },
    { "label": "Crowded long", "set": { "rate": 2 }, "note": "Everyone is long SPCX. Your long 50 SPCX pays $24 a day, and a month costs you $720 of your $1,000." },
    { "label": "Funding liquidates you", "set": { "rate": 2, "days": 60 }, "note": "Same crowded market, held for two months: funding alone eats the margin, and the venue liquidates you around day 36 without SPCX moving a cent." },
    { "label": "Paid to short", "set": { "side": "short", "rate": 2 }, "note": "The other side of the crowd: short 50 SPCX collects $24 a day." },
    { "label": "1×, crowded", "set": { "rate": 2, "leverage": 1 }, "note": "At 1× the same rate costs a fifth as much: $4.80 a day, on 10 SPCX." }
  ],
  "caption": "Funding grows in a straight line with the days held and the size of the position. A rate that looks tiny per hour is large per year, and leverage multiplies it on your dollars."
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

## Scenarios

Each scenario plays 48 hours of a made-up SPCX market against one perp
position, under a mandate. The three panels share a time axis: hover one to read all
three at that hour. The top panel is the market, the middle what the position
made, and the bottom how far it fell from its best, against the mandate's
drawdown limit.

```lab
{
  "model": "perp.scenario",
  "title": "48 hours of SPCX, under a mandate",
  "lede": "SPCX starts at $100. Choose what it does over the next two days, then size your position and set the mandate's limits.",
  "controls": [
    { "id": "path", "label": "What SPCX does", "type": "choice", "options": [["rally", "Rally"], ["chop", "Chop"], ["bleed", "Slow bleed"], ["flash-crash", "Flash crash"], ["squeeze", "Short squeeze"]], "value": "flash-crash" },
    { "id": "side", "label": "Side", "type": "choice", "options": [["long", "Long"], ["short", "Short"]], "value": "long" },
    { "id": "size", "label": "Position (SPCX)", "min": 0, "max": 120, "step": 1, "value": 50, "format": "num" },
    { "id": "leverage", "label": "Leverage", "min": 1, "max": 20, "step": 0.5, "value": 3, "format": "x" },
    { "id": "funding", "label": "Funding rate, per hour", "min": -3, "max": 3, "step": 0.05, "value": 0.15, "format": "bp" },
    { "id": "cap", "label": "Mandate: max_position", "min": 10, "max": 120, "step": 1, "value": 70, "format": "num" },
    { "id": "limit", "label": "Mandate: max_drawdown_usd", "min": 100, "max": 3000, "step": 50, "value": 1000, "format": "usd0" }
  ],
  "fixed": { "maintenance": 3 },
  "presets": [
    { "label": "Long into a flash crash", "set": { "path": "flash-crash", "side": "long", "size": 50, "leverage": 3 }, "note": "A 22% fall in an hour. The position survives the venue but breaks the drawdown limit, which no recovery undoes." },
    { "label": "Same, at 5×", "set": { "path": "flash-crash", "side": "long", "size": 40, "leverage": 5 }, "note": "At 5× the venue liquidates the position at the bottom, and it misses the rebound." },
    { "label": "Sized for the limit", "set": { "path": "flash-crash", "side": "long", "size": 40, "leverage": 3 }, "note": "40 SPCX: the same crash costs less than the drawdown limit, if only just, and the mandate holds." },
    { "label": "Short into a squeeze", "set": { "path": "squeeze", "side": "short", "size": 40, "leverage": 8 }, "note": "A short squeeze: the price jumps 20% in three hours and the short is liquidated." },
    { "label": "Paying to wait", "set": { "path": "chop", "side": "long", "size": 60, "funding": 2.5 }, "note": "A market going nowhere, with crowded funding: the position bleeds funding every hour." },
    { "label": "Over the cap", "set": { "path": "rally", "size": 90 }, "note": "A winning trade that breaks the mandate anyway: 90 SPCX against a cap of 70." }
  ],
  "caption": "The drawdown limit counts from the running peak, not from entry: a position that first gains and then gives it back can break it without ever being down overall. Sizing so that `max_drawdown_usd ÷ position` is larger than the moves you expect keeps the mandate."
}
```

## Check yourself

:::check You're long 10 SPCX from $100 at 5×. SPCX falls to $95. What is your PnL, and what share of your margin is it?
−$50. The notional was $1,000 and the margin $200, so −$50 is 25% of the
margin: 5× the 5% move.
:::

:::check Funding is +1 bp an hour and you're short $10,000 of notional. Do you pay or receive, and how much a day?
You receive. Positive funding means longs pay shorts: 0.01% × $10,000 × 24 =
$24 a day.
:::

:::check Your mandate's drawdown limit is $1,000 and you're long 50 SPCX. How far can SPCX fall from its peak before you break it?
$20: $1,000 ÷ 50 SPCX. From a peak of $110, SPCX falling below $90 breaks it. Funding paid along the way uses up some of that room.
:::

:::check Why can a winning trade still break the mandate?
Because the position cap is independent of PnL: a position over
`max_position` breaks the mandate whether or not it is making money. And the
drawdown limit counts from the peak, so giving back gains counts too.
:::

## Glossary

Every term this guide uses is in the [glossary](../glossary.md). In the text,
an underlined term shows its definition when you hover it or tap it.

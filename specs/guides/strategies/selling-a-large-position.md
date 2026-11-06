# Guide: selling a large position

Status: guide

Selling 10 SOL is one click. Selling 30,000 is a job: the book can't take it
all at once, the market moves while you work, and everyone can see you
doing it. This guide explains how professionals sell a large holding, which
the benchmark's planned treasury task tests, and how a
[mandate](../../mandate.md) keeps an agent doing it inside limits.

It assumes the [spot guide](../instruments/spot.md): SOL at **$100**, a book
of asks and bids, slippage. This time the holding is a treasury's, not
yours: **30,000 SOL**. The figures are a teaching model with made-up prices.

:::glance
- **The job:** turn a large holding into dollars, close to the price when you started
- **The score:** the shortfall, what you got against what the holding was worth at the start
- **Sell fast, and:** your own selling pushes the price down
- **Sell slowly, and:** the market moves on its own while you wait
- **Sell steadily, and:** other traders read you and get there first
- **Under a mandate:** sell only, never below a floor
:::

:::eli5
You have a cart of 3,000 apples to sell at a market where the stalls only
buy a hundred at a time. Sell them all to the first stalls and they pay less
and less for each extra hundred. Wait all day, and the price of apples may
fall before you're done.

And if you sell exactly a hundred every ten minutes, a clever trader notices,
sells his apples just before you each time, and buys them back cheaply after
you've pushed the price down. The trick is to sell steadily enough not to
crush the price, quickly enough not to be caught by the market, and
unpredictably enough not to be read.
:::

## The job

The treasury's ticket:

| Number | The sale | What it is |
| --- | --- | --- |
| **Holding** | 30,000 SOL | What there is to sell. |
| **Arrival price** | $100 | SOL's price when the sale starts. |
| **Paper value** | $3,000,000 | Holding × arrival price: what the SOL is worth, on paper, at the start. |
| **Floor** | $94 | The mandate's `min_sell_price`: no SOL may sell below it. |
| **Time** | 120 rounds | How long the sale may take; whatever is left after is sold later, at a discount. |

The paper value is a number nobody can get: selling 30,000 SOL at once would
walk far down the book, as the spot guide's 3,000 SOL did. What matters is
how close the sale comes.

### Implementation shortfall

The **implementation shortfall** is the paper value minus what the sale
actually brings in, counting whatever is left unsold at what it can be sold
for later. It's usually given in basis points of the paper value:

```
shortfall = paper value − proceeds − value of what's left
shortfall in bp = shortfall ÷ paper value × 10,000
```

| How it was sold | Proceeds | Shortfall |
| --- | --- | --- |
| All at once, at an average of $97 | $2,910,000 | $90,000: 300 bp |
| Over the session, at an average of $99.50 | $2,985,000 | $15,000: 50 bp |
| In a rising market, at an average of $101 | $3,030,000 | −$30,000: −100 bp, better than paper |

A negative shortfall means the sale beat the arrival price, usually by
waiting in a market that rose.

:::history Paper versus reality
André Perold named it in a 1988 paper, "The Implementation Shortfall: Paper
versus Reality". He compared a portfolio traded on paper, at the prices
when its decisions were made, with the same portfolio traded for real, and
found the gap between them was often bigger than the managers' skill at
picking. It became the standard way to grade a trader's execution.
:::

## Fast or slow

Two costs pull in opposite directions:

- **Impact.** Selling pushes the price down, and the faster you sell, the
  further: each round's slice walks further down the book. Some of the push
  fades as the book refills (**temporary** impact), and some stays, because
  the market has learned there's a seller (**permanent** impact).
- **Risk and drift.** While you wait, SOL moves on its own. On average it
  may drift one way, and either way the result gets less certain the longer
  the sale takes.

Selling over a few hours is usually the balance. A market that's falling
pushes the balance toward faster; one that's rising, toward slower.

```lab
{
  "model": "exec.speed",
  "title": "How fast to sell 30,000 SOL",
  "lede": "Each curve is a cost of selling evenly over the hours on the axis. Move the hours, and change the market around the sale.",
  "controls": [
    { "id": "hours", "label": "Hours to sell", "min": 0.25, "max": 48, "step": 0.25, "value": 4, "format": "h" },
    { "id": "depth", "label": "Book depth", "min": 0.25, "max": 4, "step": 0.25, "value": 1, "format": "x", "hint": "1× is normal; 4× is a book four times as deep." },
    { "id": "vol", "label": "SOL's daily moves (%)", "min": 1, "max": 10, "step": 0.5, "value": 4, "format": "num" },
    { "id": "drift", "label": "SOL's drift (% a day)", "min": -10, "max": 10, "step": 0.5, "value": 0, "format": "num", "hint": "Negative: SOL falls while you sell." }
  ],
  "presets": [
    { "label": "A balanced sale", "set": {}, "note": "4 hours: $15,000 of impact (50 bp), and a bad day could cost about $28,000 more. The best balance here is about 2.5 hours." },
    { "label": "Dump in 15 minutes", "set": { "hours": 0.25 }, "note": "Selling in 15 minutes costs $127,500 of impact, 425 bp: the book can't absorb it." },
    { "label": "Take a day", "set": { "hours": 24 }, "note": "A day costs only 29 bp of impact, but one bad day's move is worth about $69,000." },
    { "label": "SOL is falling", "set": { "drift": -5 }, "note": "SOL falling 5% a day adds $12,500 of drift to a 4-hour sale, and the balance moves to about 2 hours." },
    { "label": "SOL is rising", "set": { "drift": 5 }, "note": "A rising market pays you to wait: the drift is a gain, and the balance moves out to the end of the axis." },
    { "label": "A deep book", "set": { "depth": 4 }, "note": "A book four times as deep makes speed cheaper, and the balance moves in to about an hour." }
  ],
  "caption": "Impact falls as the sale slows; risk grows with the square root of the time. The dashed curve adds one standard deviation of risk to the expected cost, and its lowest point is the balance this lab calls best."
}
```

## Schedules

A **schedule** says how much to sell when. Four simple ones, each the
winner in some market:

| Schedule | How it sells | It wins when |
| --- | --- | --- |
| **All at once** | Everything in the first round, down the book | The market is about to crash: any SOL left will sell for less |
| **Evenly (TWAP)** | The same slice every round: 250 SOL | The market is calm, and nobody is watching |
| **Front-loaded** | Twice the pace, done halfway | The market is falling steadily |
| **Adaptive** | Faster when SOL is under the arrival price, slower when over, then whatever is left at the end | The market is rising, or someone is reading steady sellers |

**TWAP** stands for time-weighted average price: selling the same amount in
each period gets you, roughly, the market's average price over the session.

## Being read

On-chain, every order is public. A seller who sells 250 SOL every round is a
pattern, and a **front-runner** can trade on it: sell just ahead of each
slice, let the slice push the price down, and buy back lower. Every slice
then sells into a price the front-runner has already pushed down.

Steady schedules pay for it most. Ways to be harder to read:

- vary the slices' sizes and timing;
- rest asks on the book instead of always taking the bids;
- split the sale across venues, such as OpenBook and Phoenix;
- pause after being traded ahead of, so the front-runner's trade doesn't pay.

:::history Why "front-running"
The usual story is that it began on trading floors: a broker who saw a
client's large order coming would run to the front of the crowd and trade
for himself first. On a broker it's illegal, because the order was a
client's secret. On a public blockchain, where every order is visible to
all, reading the flow is just trading.
:::

## Under a mandate

A treasury's mandate, in [mandate v1](../../mandate.md#examples), allows the
SOL-USDC spot markets on OpenBook and Phoenix, sell-only, never below $94:

- **`sides: ["sell"]`**: no buys, and no resting bids, even to buy back
  cheaply after a crash.
- **`min_sell_price: 94`**: no fill below $94, including the last SOL of a
  slice that walks the book. A schedule must stop its slices at the floor.
- **`max_position: 30000`**: the treasury's holding, counted across both
  venues and the wallet.
- **No drawdown limit**: the holding falls with SOL whatever the agent does.

The floor changes the job. In a fall, the price can sink to the floor before
the sale is done, and a schedule that waited can't sell what's left. In a
crash, the floor arrives within a few rounds.

## Scenarios

Each scenario plays a 120-round session of a made-up SOL market against the
four schedules, under the mandate's floor. The top panel is SOL's price
before anyone's selling, and the bottom one what each schedule still has to
sell. The readouts are each schedule's shortfall: lower is better, and
negative means it sold above $100.

```lab
{
  "model": "exec.session",
  "title": "Four schedules, five markets",
  "lede": "Choose the market, the floor and the book. Unsold SOL at the end is valued 3% under the closing price, as the treasury would sell it later.",
  "controls": [
    { "id": "regime", "label": "What SOL does", "type": "choice", "options": [["calm", "Calm"], ["fall", "Falls"], ["climb", "Climbs"], ["crash", "Crashes"], ["shakeout", "Dips, then climbs"]], "value": "calm" },
    { "id": "reader", "label": "A front-runner", "type": "choice", "options": [["off", "Absent"], ["on", "Watching"]], "value": "off" },
    { "id": "floor", "label": "Floor, % under $100", "min": 1, "max": 10, "step": 0.5, "value": 6, "format": "num", "hint": "The mandate's min_sell_price: 6% is $94." },
    { "id": "depth", "label": "Book depth", "min": 0.25, "max": 4, "step": 0.25, "value": 1, "format": "x" }
  ],
  "presets": [
    { "label": "Calm", "set": {}, "note": "In a calm market, evenly wins at 2 bp; all at once costs 300 bp." },
    { "label": "Calm, being read", "set": { "reader": "on" }, "note": "A front-runner takes 30 bp a round from steady sellers: evenly falls to 30 bp, and adaptive, whose pace varies, wins at 18 bp." },
    { "label": "A steady fall", "set": { "regime": "fall" }, "note": "SOL falls 8%. Front-loaded wins at 256 bp; evenly reaches the floor with 2,127 SOL unsold, and does worst." },
    { "label": "A climb", "set": { "regime": "climb" }, "note": "SOL climbs 6%. Adaptive slows down and sells high: 322 bp better than paper." },
    { "label": "A crash", "set": { "regime": "crash" }, "note": "SOL drops 10% in five rounds. Selling everything at once, at 300 bp, beats every schedule that waited." },
    { "label": "Dips, then climbs", "set": { "regime": "shakeout" }, "note": "A dip that turns into a climb. Adaptive speeds up into the dip and does worst, at 427 bp; evenly wins at 231 bp." }
  ],
  "caption": "No schedule wins every market. A good seller reads which market it's in, from the price, the book and the tape, and changes pace: fast when it falls, patient when it rises, irregular when it's watched."
}
```

## Parameters

What an execution bot decides before it sells:

| Parameter | What it means |
| --- | --- |
| Horizon | How long the sale may take, and what happens to what's left. |
| Pace | How much to sell in each round, and how it changes with the market. |
| Urgency | How fast to speed up when the price falls, and slow down when it rises. |
| Passive share | How much to rest as asks, which pay less impact and fee but may not fill, against selling into bids. |
| Venue split | How much to sell on each venue, from each book's depth. |
| Disguise | How much to vary slice sizes and timing, and when to pause. |
| The floor | How to stop each slice before it walks below `min_sell_price`. |

## Risks

:::group
::risk Impact
Selling too fast walks the book, and the price you push down stays partly
down.
::
::risk The market
Selling too slowly leaves you holding through whatever SOL does next.
::
::risk Being read
A steady schedule is a signal, and front-runners trade against it.
::
::risk The floor
In a fall the floor can stop the sale with SOL still unsold, to be sold
later for less.
::
::risk Breaking the mandate
One slice that walks below the floor, or one resting bid on a sell-only
mandate, breaks it.
::
::risk Reading the market wrong
An adaptive rule that sells faster in a dip sells the bottom of a dip that
turns into a climb.
::
:::

## When to use it, and when not to

:::group
::use
- The holding is **large next to the book**: more than its first few
  levels.
- A **benchmark price** matters, such as the price when the decision was
  made.
- The market is **public**, and others can read your orders.
::
::avoid
- The holding is **small**: one market order costs less than the effort.
- The sale is **urgent**: a crash, or a deadline, makes waiting the risk.
- The price is about to **move against you** for certain: then speed beats
  care.
::
:::

## Check yourself

Three questions for each idea, all on the treasury's 30,000 SOL. Work each
one out on the ticket first (holding, arrival price, paper value, floor),
then open the answer.

### The shortfall

:::check You sell all 30,000 SOL at an average of $98.50. What's the shortfall?
$45,000, or 150 bp. The paper value is $3,000,000, and the proceeds
$2,955,000.
:::

:::check You sell 20,000 SOL at an average of $99, and 10,000 are left. SOL closes at $96, and what's left is valued 3% under the close. What's the shortfall?
$88,800, or 296 bp. The proceeds are $1,980,000, and the 10,000 SOL left
are worth 10,000 × $96 × 0.97 = $931,200.
:::

:::check Your shortfall is −100 bp. What happened?
You sold above the arrival price, on average at $101: usually by waiting in
a market that rose.
:::

### Fast or slow

:::check In the lab, why does selling in 15 minutes cost 425 bp of impact when 4 hours costs 50?
Because impact grows with speed: you sell sixteen times as fast, so each
moment's selling walks much further down the book.
:::

:::check Why not take two days and pay almost no impact?
Because the longer the sale, the more SOL can move against you. In the lab,
one bad day's move over a day-long sale is worth about $69,000.
:::

:::check SOL is falling 5% a day. Should you sell faster or slower?
Faster. Every hour you wait, the SOL you still hold is worth less; in the
lab the balance moves from 2.5 to about 2 hours.
:::

### Being read

:::check You sell exactly 250 SOL every round. What does a front-runner do?
It sells just before each of your slices, lets your slice push the price
down, and buys back lower. Every slice sells into a price it already pushed
down.
:::

:::check How can a bot be harder to read?
Vary slice sizes and timing, rest some of the sale as asks, split it across
venues, and pause after being traded ahead of.
:::

:::check In a calm market with a front-runner watching, why does adaptive beat evenly in the lab?
Because adaptive's pace changes with the price, the front-runner can read
only part of it: it loses 18 bp, against evenly's 30.
:::

### The mandate

:::check The floor is $94, and the last SOL of your next slice would sell at $93.90. May you sell it?
No. Every fill must be at $94 or above, not just the average, so the slice
has to stop at the floor.
:::

:::check SOL falls 8% over the session. Why can evenly end with SOL unsold?
Because SOL reaches the floor before evenly is done, and it may not sell
below it. In the lab it's left with 2,127 SOL.
:::

:::check SOL crashes, then recovers. May the agent buy some SOL back at the bottom?
No. The mandate is sell-only, and that covers resting bids too.
:::

## Glossary

Every term this guide uses is in the [glossary](../glossary.md). In the text,
an underlined term shows its definition when you hover it or tap it.

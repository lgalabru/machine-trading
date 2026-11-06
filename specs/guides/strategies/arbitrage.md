# Guide: arbitrage

Status: guide

Arbitrage is the simplest trading strategy to state: when the same thing
has two prices, buy it where it is cheap and sell it where it is dear. If
both trades go through, the profit doesn't depend on which way the price
moves next. This guide explains the two kinds an agent can run with
perps, shows where the profit really comes from, and covers the ways a
"riskless" trade loses money.

It assumes the [perp guide](../instruments/perp.md): the same $1,000 and
SPCX at $100. The figures are a teaching model with made-up prices.

:::glance
- **The idea:** the same thing has two prices, or pays you to hold it
- **You hold:** two opposite positions, so a price move doesn't matter
- **You earn:** the gap between the prices, or funding
- **You need:** two places to trade, or two ways to hold the same thing
- **It ends when:** the gap closes, often because traders like you closed it
- **Worst case:** one side of the trade fails, and you're left holding the other
:::

:::eli5
Your school's shop sells pencils for 10 cents, and the shop across the
street buys them for 12. You buy a pencil here and sell it there: 2 cents,
whichever way pencil prices go tomorrow.

Soon other kids do the same. Your school's shop runs low and raises its
price, and the shop across the street gets so many pencils it pays less. The
gap shrinks until it's no longer worth crossing the street. Arbitrageurs are
paid for making two prices agree.
:::

## What arbitrage is

An arbitrage holds two positions, called **legs**, that cancel each other's
exposure to the price. Long 10 SPCX in one place and short 10 SPCX in
another is **delta-neutral**: if SPCX rises $1, one leg makes $10 and the
other loses $10. What's left is the reason for the trade: a gap between the
two prices, or a payment one leg receives, such as funding.

Every arbitrage answers the same three questions:

1. **What is the gap?** A difference in price, or a payment like funding.
2. **What does it cost to capture?** Fees, slippage, and the margin or
   capital tied up while you hold.
3. **What can break the hedge?** A leg that doesn't fill, a leg that's
   liquidated, a gap that widens before it closes.

:::history Where the word comes from
"Arbitrage" is French for arbitration: judging between two things. In the
1700s, merchants' manuals taught the "arbitration of exchanges": comparing
the rates at which bills of exchange traded in Amsterdam, London and Paris
to find the cheapest route for money between them. The word kept the job.
:::

## Two prices: cross-venue arbitrage

SPCX's perp trades at $100.00 on one venue and $100.40 on another. You buy
on the cheap one and sell on the dear one, at the same moment. You are now
long and short the same amount, so price moves cancel, and you've locked in
$0.40 a unit.

Locked in, minus costs:

- **Fees.** Each trade pays the venue a fee. At 2 bp a trade, buying and
  selling $100 of SPCX costs about $0.04 a unit.
- **Slippage.** Each venue's book only has so much SPCX near its price. The
  more you trade, the further down the book you go, and the worse your
  average price. Slippage grows with size faster than the profit does.

So every gap has a best size: big enough to be worth it, small enough that
your own trades don't eat the gap.

```lab
{
  "model": "arb.spread",
  "title": "How much to trade when SPCX has two prices",
  "lede": "SPCX is $100.00 on one venue. Set how much dearer the other is, what each trade costs, and how deep the books are. The curve is your profit at each size.",
  "controls": [
    { "id": "gap", "label": "Gap between the venues", "min": 0, "max": 1, "step": 0.01, "value": 0.4, "format": "price", "hint": "SPCX is $100 plus this on the dearer venue." },
    { "id": "fee", "label": "Fee per trade", "min": 0, "max": 10, "step": 0.5, "value": 2, "format": "bp", "hint": "You pay it twice: once on each venue." },
    { "id": "depth", "label": "Slippage", "min": 0.1, "max": 5, "step": 0.1, "value": 1, "format": "num", "hint": "Cents each venue's price moves against you for every 10 SPCX you trade." },
    { "id": "size", "label": "You trade (SPCX)", "min": 0, "max": 400, "step": 5, "value": 100, "format": "num" }
  ],
  "presets": [
    { "label": "A good gap", "set": {}, "note": "$0.40 a unit, $0.04 of fees: the best size is 180 SPCX, for about $32." },
    { "label": "Gap smaller than fees", "set": { "gap": 0.03 }, "note": "3 cents of gap and 4 cents of fees: every size loses money. Most gaps you'll see are like this." },
    { "label": "Thin books", "set": { "depth": 4 }, "note": "The same gap with shallow books: the best size drops to about 45 SPCX, for $8, and past 90 your own trades have eaten the gap." },
    { "label": "Too big", "set": { "size": 400 }, "note": "Trading 400 SPCX pushes both prices so far that the trade loses money, on a gap that would have paid at 180." }
  ],
  "caption": "The profit is the gap times the size, minus fees, which grow in line with the size, minus slippage, which grows with the square of it. The peak is the best size, and past the break-even size you're paying to trade."
}
```

:::note Why gaps are small and short-lived
Many bots watch the same two prices, and the first to trade takes the gap.
On-chain, they compete for the same block, so a gap large enough to see is
often gone by the time a slower bot's transaction lands. Speed and low fees
decide who earns cross-venue arbitrage.
:::

## Funding: cash and carry

The second kind needs no second price, only patience. When a perp's funding
is positive, longs pay shorts. So you **short the perp** and collect
funding, and you **buy the same amount of SPCX itself** (spot) so the price
can't hurt you. This is called **cash and carry**, or a basis trade.

With your $1,000, the trade is two tickets that share it. The perp ticket
has the same numbers as in the [perp guide](../instruments/perp.md):

| Number | Spot leg | Perp leg | What it is |
| --- | --- | --- | --- |
| **SPCX price** | $100 | $100 | Both legs open at the same price. |
| **Committed** | $833 | $167 of margin | Your $1,000, split so both legs hold the same SPCX. |
| **Leverage** | 1× | 5× | Spot is never leveraged; you choose the perp's. |
| **Notional** | $833 | $833 | $167 × 5 on the perp. |
| **Position** | +8.3 SPCX | −8.3 SPCX | Long on one side, short on the other: they cancel. |

And the ledger, as SPCX moves, before funding:

| SPCX | Spot leg PnL | Perp leg PnL | Together |
| --- | --- | --- | --- |
| $110 | +$83 | −$83 | $0 |
| $100 | $0 | $0 | $0 |
| $90 | −$83 | +$83 | $0 |

What's left is funding: every hour, the perp leg collects it on its $833
notional.

The catch is that the legs are in two places. The spot leg's gains sit in
your SPCX, while the short leg's losses eat its margin. In a rally the short
leg can be liquidated, even though your total is fine, unless you move
dollars to it in time. Once it is liquidated, you're simply long SPCX, the
bet you set out to avoid.

```lab
{
  "model": "arb.carry",
  "title": "Collecting funding with your $1,000",
  "lede": "Pick what SPCX does over 30 days, the funding rate, and the short leg's leverage. The middle panel splits your PnL into the two legs and the funding.",
  "controls": [
    { "id": "path", "label": "What SPCX does", "type": "choice", "options": [["flat", "Drifts"], ["rally", "Rallies 30%"], ["crash", "Falls 30%"], ["spike", "Spikes, then back"]], "value": "flat" },
    { "id": "rate", "label": "Funding rate, per hour", "min": -2, "max": 3, "step": 0.05, "value": 1, "format": "bp", "hint": "Positive: longs pay shorts, so your short collects." },
    { "id": "leverage", "label": "Short leg leverage", "min": 1, "max": 10, "step": 0.5, "value": 5, "format": "x", "hint": "More leverage puts more dollars into SPCX, and the short leg closer to liquidation." }
  ],
  "fixed": { "maintenance": 3, "capital": 1000, "entry": 100, "days": 30 },
  "presets": [
    { "label": "The carry works", "set": {}, "note": "SPCX drifts, the legs cancel, and the short collects about 1 bp an hour: about $60 in a month on $1,000." },
    { "label": "Rally at 5×", "set": { "path": "rally" }, "note": "SPCX rises 30%. The short leg is liquidated near $122 (the funding it collected bought it a little room), leaving you long SPCX, unhedged. SPCX kept rising, so the month ends up: luck, not the trade. A fall from there would have been all loss." },
    { "label": "Rally at 2×", "set": { "path": "rally", "leverage": 2 }, "note": "Less leverage: the short leg survives the rally, the hedge holds, and the carry is earned on fewer SPCX." },
    { "label": "Spike", "set": { "path": "spike" }, "note": "A one-day 25% spike that fully reverses: the short leg is liquidated at the top, and the spot leg's gain vanishes on the way back down." },
    { "label": "Funding flips", "set": { "rate": -0.5 }, "note": "The crowd turned short: now your short pays funding every hour, and the trade loses money slowly." }
  ],
  "caption": "While both legs stand, price moves cancel and only funding is left. The risk is the short leg's margin: the spot leg's gains can't pay for it unless you move dollars across in time."
}
```

:::history Arbitrage isn't riskless
Long-Term Capital Management, a hedge fund run by star traders and two Nobel
laureates, made its money on convergence trades: pairs of bonds whose prices
had to come together eventually. In 1998, after Russia defaulted, the gaps
widened instead of closing. The fund was so leveraged that it couldn't
wait, and the Federal Reserve organised a rescue by its banks. The trades
were mostly right. The fund didn't survive long enough to find out.
:::

## What can go wrong

:::group
::risk Legging risk
You buy on one venue, and before your sale lands on the other the gap
closes. Now you hold one leg, unhedged, with the price free to move.
::
::risk Liquidation of one leg
The two legs' gains and losses sit in different accounts. A short perp can be
liquidated in a rally while the spot leg is up just as much.
::
::risk Costs eat the gap
Fees, slippage, transaction fees and the cost of moving dollars between
venues can each be bigger than the gap.
::
::risk Funding changes
Funding is paid at whatever the rate is, not the rate when you entered. A
crowded long can turn into a crowded short in days.
::
::risk Competition
Other bots see the same gap. What's left for you is the gap after them,
which is often nothing.
::
::risk Venue risk
Arbitrage spreads your dollars across two venues, so it carries both
venues' risks: their software, oracles and solvency.
::
:::

## When to use it, and when not to

:::group
::use
- Two prices for the same thing differ by **more than the fees and
  slippage** of closing the gap.
- Funding is **reliably positive**, and you can hold the spot leg.
- You want returns that **don't depend on the price**'s direction.
- You can trade **fast**, and **watch both legs**, moving margin as needed.
::
::avoid
- The gap is **smaller than your round-trip costs**.
- You can't trade both legs **at the same moment**, or close to it.
- The short leg would need **high leverage**, leaving it near liquidation.
- Funding **swings** from positive to negative from one day to the next.
- Moving dollars between the venues is **slow**.
::
:::

## Parameters

The numbers an arbitrage bot needs before it trades:

| Parameter | What it means |
| --- | --- |
| Entry threshold | The smallest gap, after costs, worth trading. Below it, do nothing. |
| Size | How much to trade, from the gap, the books' depth and the position cap. |
| Costs | Fees on each venue, expected slippage, and transaction fees. |
| Leg leverage | The margin each perp leg gets, which sets how far the price can move before it's liquidated. |
| Exit rule | When to unwind: the gap closed, funding turned, or a leg needs margin it can't get. |

## Arbitrage under a mandate

The [mandate](../../mandate.md) can hold both kinds: it allows `spot`
instruments, and a universe that spans several venues, with the account's
value added up across them.

**Cross-venue arbitrage** on SOL allows the same pair on two venues, both
sides:

```json
{
  "version": "v0",
  "universe": [
    { "instrument": "openbook:spot:SOL-USDC", "max_position": 100 },
    { "instrument": "phoenix:spot:SOL-USDC", "max_position": 100 }
  ],
  "limits": { "max_drawdown_usd": 500 }
}
```

Both allowances measure the same SOL, held wherever it is, so the cap bounds
the SOL the agent has bought and not yet sold, across both venues.

**Cash and carry** pairs a spot leg with a perp leg on the same asset:
`openbook:spot:SOL-USDC` long and `phoenix:perp:SOL` short. The spot
position is the SOL held; the perp position is the short. Setting both caps
equal keeps the agent from holding more on one leg than the other, and the
drawdown limit covers the two legs together, since the account's value adds
them up.

Neither mandate says the legs must match: it bounds what the agent may hold,
not how it hedges. A short perp leg left without its spot leg is within both
caps, and only the drawdown limit catches what it costs.

## Check yourself

:::check SPCX is $100.00 on one venue and $100.05 on another, and each trade costs 3 bp. Should you trade the gap?
No. Buying and selling $100 of SPCX costs 3 bp twice, about $0.06 a unit,
more than the $0.05 gap, before any slippage.
:::

:::check You're long 10 SPCX spot and short 10 SPCX on the perp. SPCX falls $8. What's your PnL, before funding?
Zero. The spot leg loses $80 and the short gains $80.
:::

:::check In cash and carry, why can a rally hurt you even though the legs cancel?
The short leg's losses come out of its margin on the perp venue, while the
spot leg's gains stay in SPCX. If the short leg is liquidated, you're left
long SPCX alone, with no hedge.
:::

:::check Who closes an arbitrage gap?
The arbitrageurs. Buying where it's cheap pushes that price up, and selling
where it's dear pushes that one down, until the gap is smaller than the cost
of trading it.
:::

## Glossary

Every term this guide uses is in the [glossary](../glossary.md). In the text,
an underlined term shows its definition when you hover it or tap it.

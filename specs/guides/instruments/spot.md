# Guide: spot (`spot`)

Status: guide

This guide explains spot trading to someone who has never placed an order:
what you hold, what a trade costs, where your tokens sit, and how a
[mandate](../../mandate.md) bounds an agent trading spot. The
[spot profile](../../instruments/spot.md) is the normative text; when this
guide and the profile disagree, the profile is right.

It builds one idea at a time, on the perp guide's example: you have
**$1,000**, and SPCX trades at **$100**. Buying SPCX here is what the perp
guide called "buying outright": owning SPCX itself, instead of a contract
on its price. The `SPCX-USDC` market in this guide is illustrative;
the spot markets the specs name today trade SOL for USDC, and work the same
way. The figures use made-up prices and
a simplified book; the [OpenBook](../../venues/openbook.md) and
[Phoenix](../../venues/phoenix.md) venue profiles define the real ones.

:::glance
- **You buy:** the thing itself, such as SPCX, and it's yours
- **You pay:** the full price, in another token, such as USDC
- **You gain or lose:** what the thing gains or loses, no more
- **Nothing to watch:** no margin, no liquidation, no funding, no expiry
- **You can't:** sell what you don't have
- **Worst case:** the thing becomes worth nothing, and so does what you paid
:::

:::eli5
You go to the market with $10 and buy apples. You take the apples home:
they're yours. If apples get dearer, your apples are worth more; if they get
cheaper, less. Nobody can take them back, and nobody charges you for keeping
them.

The stall has a price to buy at and a slightly lower price it will buy them
back at, and that gap is how the stall earns its living. If you want a lot of
apples at once, the stall runs out of cheap ones and starts selling you the
dearer ones from the back.

The apples are SPCX, your $10 is USDC, the stall is the order book, and the
gap is the spread.
:::

## What spot is

A spot trade swaps one asset for another, now. You buy SPCX by paying USDC,
and the SPCX arrives in your account as soon as the trade happens. You own it:
there is no contract, no counterparty you still depend on, and nothing that
runs out.

That makes spot the simplest instrument there is. What you hold is what you
bought, and its value is what it's worth. The things that make a perp
dangerous (margin, liquidation, funding) don't exist here.

:::history Why "spot"
The word is old commodity-market English. A spot sale was paid for and
delivered on the spot, as opposed to a forward sale agreed now and settled
later. Futures, and later perps, grew out of the forward side; spot is the
original trade.
:::

### The pair: base and quote

A spot market trades a **pair** of assets, written base-quote: `SPCX-USDC`
here, or `SOL-USDC` on the markets the specs name.

| | In `SPCX-USDC` | What it is |
| --- | --- | --- |
| **Base** | SPCX | What the market trades. Your position is counted in it. |
| **Quote** | USDC | What prices are in. A price of $100 means 100 USDC per SPCX. |

Buying gives you base and costs you quote; selling does the reverse. USDC is
a token worth one dollar, so the guide writes its amounts in dollars.

### Your first trade

You buy SPCX with your $1,000. For now, ignore what the trade itself costs;
the [next section](#the-order-book) adds it. The trade comes down to three
numbers:

| Number | Your trade | What it is |
| --- | --- | --- |
| **SPCX price** | $100 | What one SPCX costs. |
| **You spend** | $1,000 | Your USDC, all of it. You pay the full price: there is no margin. |
| **Position** | 10 SPCX | What you hold: $1,000 ÷ $100. |

### As SPCX moves

Your holding is worth your position times the price, and nothing else
changes it:

| SPCX | Your 10 SPCX are worth | Change |
| --- | --- | --- |
| $120 | $1,200 | +$200 |
| $100 | $1,000 | $0 |
| $80 | $800 | −$200 |
| $50 | $500 | −$500 |

At $50 you've lost half, and you still hold 10 SPCX: nobody closes your
position, and if SPCX comes back, so does your money. The perp guide's long
at 5× was liquidated near $82; the same idea at 1× moves exactly like these
10 SPCX, minus funding.

```lab
{
  "model": "spot.compare",
  "title": "Your $1,000 in SPCX: spot against a perp",
  "lede": "Drag where SPCX goes. The blue line is 10 SPCX bought outright; the orange one the same $1,000 as margin on a perp long, at the leverage you choose.",
  "controls": [
    { "id": "exit", "label": "SPCX moves to", "min": 40, "max": 160, "step": 1, "value": 80, "format": "price" },
    { "id": "leverage", "label": "Perp leverage", "min": 1, "max": 10, "step": 0.5, "value": 5, "format": "x" },
    { "id": "days", "label": "Days held", "min": 1, "max": 90, "step": 1, "value": 30, "format": "d" },
    { "id": "funding", "label": "Perp funding, per hour", "min": -1, "max": 2, "step": 0.05, "value": 0.15, "format": "bp", "hint": "Positive: longs pay shorts." }
  ],
  "fixed": { "deposit": 1000, "maintenance": 3 },
  "presets": [
    { "label": "SPCX falls to $80", "set": {}, "note": "The 10 SPCX are worth $800. The 5× perp was liquidated near $82 on the way down." },
    { "label": "SPCX halves", "set": { "exit": 50 }, "note": "Spot has lost half and still holds 10 SPCX. A perp at 2× or more is gone: even 2× is liquidated near $51.55." },
    { "label": "SPCX rises to $120", "set": { "exit": 120 }, "note": "Spot makes $200. The 5× perp makes about $1,000, less $54 of funding: leverage cuts both ways." },
    { "label": "Perp at 1×", "set": { "leverage": 1, "exit": 120 }, "note": "At 1× the perp tracks spot, minus the funding it pays to hold: $10.80 a month here." }
  ],
  "caption": "Spot is the straight line through $1,000 at $100: no floor except zero, and no cost of holding. A perp can multiply the line, but pays funding and can be closed for you."
}
```

## The order book

A spot venue like OpenBook or Phoenix matches buyers and sellers in an
**order book**: a list of offers to sell (**asks**) and offers to buy
(**bids**), by price. The cheapest ask and the dearest bid sit either side of
the **mid**, and the gap between them is the **spread**.

| | Price | SPCX on offer |
| --- | --- | --- |
| Ask | $100.15 | 100 |
| Ask | $100.10 | 100 |
| Ask (best) | $100.05 | 100 |
| *Mid* | *$100.00* | |
| Bid (best) | $99.95 | 100 |
| Bid | $99.90 | 100 |

You can trade with it two ways:

- A **market order** takes what's on offer now: a buy fills at the asks,
  cheapest first, and a sell at the bids. It always fills, at whatever the
  book has.
- A **limit order** names your price and **rests** on the book until someone
  trades with it. It never pays more than you said, but it may never fill.

Each fill also pays the venue a **fee**, a small share of what you traded,
usually less for the resting side (the **maker**) than for the side that
took it (the **taker**).

So the real ticket for your first trade, a market buy with a 2 bp taker fee:

| Number | Your trade | What it is |
| --- | --- | --- |
| **SPCX price** | $100.05 | The best ask: the mid plus half the $0.10 spread. |
| **You spend** | $1,000 | Your USDC. |
| **Fee** | $0.20 | 2 bp of $1,000. |
| **Position** | 9.993 SPCX | ($1,000 − $0.20) ÷ $100.05. |

For your 10 SPCX, the book's first level is deep enough, and the whole cost is
about $0.70. A big order is different: it uses up the first level and moves
on to dearer ones, so its average price gets worse the more it buys. That
difference from the mid is **slippage**.

```lab
{
  "model": "spot.book",
  "title": "What a trade costs on the book",
  "lede": "SPCX's mid is $100 and the book has the same size at every $0.05 level. Choose how much to trade and how; the curve is the average price for every size.",
  "controls": [
    { "id": "side", "label": "You", "type": "choice", "options": [["buy", "Buy"], ["sell", "Sell"]], "value": "buy" },
    { "id": "order", "label": "Order", "type": "choice", "options": [["market", "Market"], ["limit", "Limit"]], "value": "market" },
    { "id": "size", "label": "SPCX", "min": 1, "max": 3000, "step": 1, "value": 10, "format": "num" },
    { "id": "depth", "label": "SPCX at each level", "min": 10, "max": 1000, "step": 10, "value": 100, "format": "num", "hint": "A deep book has more." },
    { "id": "spread", "label": "Spread (cents)", "min": 2, "max": 50, "step": 1, "value": 10, "format": "num" },
    { "id": "fee", "label": "Fee per trade", "min": 0, "max": 10, "step": 0.5, "value": 2, "format": "bp" }
  ],
  "presets": [
    { "label": "Your 10 SPCX", "set": {}, "note": "10 SPCX fit in the first level: $100.05 each, 5 bp over the mid, and $0.20 of fees." },
    { "label": "250 SPCX", "set": { "size": 250 }, "note": "250 SPCX take three levels: $100.09 on average, and the last SPCX costs $100.15." },
    { "label": "3,000 SPCX, thin book", "set": { "size": 3000, "depth": 50 }, "note": "3,000 SPCX on a thin book walk 60 levels: the last SPCX costs $103.00, and the average is $101.53." },
    { "label": "A limit order instead", "set": { "order": "limit", "size": 250 }, "note": "A bid at $99.95 pays no spread and no slippage, but only fills if a seller comes down to it." },
    { "label": "Selling 3,000", "set": { "side": "sell", "size": 3000, "depth": 50 }, "note": "Selling is the mirror image: the book's bids run out, and the last SPCX sells for $97.00." }
  ],
  "caption": "For a small order the cost is the half-spread and the fee. For a large one, slippage takes over, and it grows with the size: each SPCX more goes further down the book."
}
```

## Where your SPCX is

On a Solana venue, your tokens move between your **wallet** and the
venue's accounts as you trade. They are yours wherever they are, and the
spot profile counts them all as your position:

| Where | What sits there | Free to move? |
| --- | --- | --- |
| **Wallet** | Your SPCX and your USDC, as tokens | Yes |
| **On the venue, free** | What your fills paid you, not yet withdrawn: on OpenBook in your *open orders account*, on Phoenix in your *seat* | Yes, once you **settle** it back to the wallet |
| **On the venue, locked** | What your resting orders offer: SPCX behind your asks, USDC behind your bids | No, until the order fills or you cancel it |

Say you hold 10 SPCX and rest an ask for 4 of them on OpenBook. Your wallet
has 6 SPCX, 4 are locked on OpenBook, and your position is still **10 SPCX**:
you haven't sold anything until the ask fills.

Your wallet also holds some SOL, which pays your transaction fees. It isn't
part of your position, and on a SOL-USDC market the profiles count only
wrapped SOL, the token form of SOL that venues trade, for the same reason.

## Parameters

What a spot market is, as the specs and a venue describe it:

| Parameter | Who sets it | What it means |
| --- | --- | --- |
| Instrument reference | The mandate | `<venue>:spot:<symbol>`, such as `openbook:spot:SOL-USDC`, a market the venue profile names. |
| `max_position` | The mandate | The most of the base asset the account may hold, wherever it is. |
| `sides` | The mandate | Whether the agent may buy, sell, or both. |
| `min_sell_price`, `max_buy_price` | The mandate | The worst prices at which the agent may sell or buy. |
| Market address, base and quote | The venue profile | Which market a symbol names, and its two tokens. |
| Reference prices | The venue profile | The prices holdings are valued at, such as a Pyth price feed. |
| Lot size and tick size | The venue | The smallest step in size and in price an order can take. |
| Fees | The venue | What each fill costs, as maker and as taker. |

## Risks

:::group
::risk Price risk
You hold the whole asset, so you carry its whole fall. There's no
liquidation to stop a loss, and no floor but zero.
::
::risk Slippage
A large order moves the price against itself as it walks the book. The
bigger the order and the thinner the book, the worse the average price.
::
::risk Being read
On-chain, every order is public before and after it lands. A trader who
sees a steady seller can sell ahead of it, or rest just in front of its
asks.
::
::risk Tokens on the venue
Free and locked balances sit in the venue's accounts until you settle
them. The venue's program, and its upgrade authority, are part of the trade.
::
::risk The quote asset
Prices are in USDC, which is meant to be worth $1. If it isn't, everything
you sold for it is worth less too.
::
::risk Missing the fill
A limit order avoids the spread but may never fill, and the price may move
away while it waits.
::
:::

## When to use it, and when not to

:::group
::use
- You want to **own** the asset: to hold it, transfer it, stake it.
- You want exposure **without leverage**, and nothing that can close your
  position for you.
- You plan to hold for a **long time**, with no funding to pay.
- You need to **turn one asset into another**, such as a treasury selling
  SPCX for dollars.
::
::avoid
- You want to bet on a **fall**: spot can only sell what you hold.
- You want **more exposure** than your dollars buy.
- Your order is **large next to the book**, and you'd trade it all at once.
- You can't **hold the tokens** safely between trades.
::
:::

## Spot under a mandate

[Mandate v1](../../mandate.md) adds spot. For a spot instrument:

- **The position is your holdings of the base asset**, wherever they are:
  wallet, free and locked on every venue. Two allowances on SPCX, one on
  OpenBook and one on Phoenix, limit the same SPCX.
- **`sides`** says whether the agent may buy, sell, or both. It covers
  resting orders too: under a sell-only mandate, a bid that never fills
  still breaks it.
- **`min_sell_price`** and **`max_buy_price`** limit every fill, not the
  average: one SPCX sold below the floor breaks the mandate.
- **The drawdown limit is optional.** A mandate to sell SPCX usually has
  none, since the holding falls with SPCX whatever the agent does.

:::example A treasury sale
A treasury holds 30,000 SOL and wants dollars, without selling below $94.
Its mandate allows `openbook:spot:SOL-USDC` and `phoenix:spot:SOL-USDC`, both
`sides: ["sell"]` and `min_sell_price: 94`, with `max_position: 30000`. The
[mandate](../../mandate.md#examples) spells it out. How to sell 30,000 SOL
without walking the book, or being read, is the subject of the
[selling a large position](../strategies/selling-a-large-position.md) guide.
:::

## Check yourself

Three questions for each idea, all on SPCX. Work each one out on the ticket
first (price, what you spend, position), then open the answer.

### Owning SPCX

:::check You buy SPCX at $100 with $1,000, ignoring costs. SPCX rises to $120. What are your SPCX worth?
$1,200. You hold 10 SPCX, and 10 × $120 = $1,200.
:::

:::check You hold 10 SPCX, bought at $100. SPCX falls to $50. Are you liquidated?
No. Spot can't be liquidated: you hold 10 SPCX worth $500, and if SPCX comes
back, so does your money. A perp long at 5× would have been closed near $82.
:::

:::check You think SPCX will fall and you hold no SPCX. Can you bet on it with spot?
No. Spot only sells what you hold. To gain from a fall you'd short a perp,
or borrow SPCX to sell, which is a money market, not spot.
:::

### The order book

:::check The best bid is $99.95, the best ask $100.05, and the fee 2 bp. You buy 10 SPCX with a market order and sell them straight back. What did the round trip cost?
$1.40. You pay 10 × $100.05 plus 2 bp ($1,000.70), and receive 10 × $99.95
less 2 bp ($999.30): the spread is $1.00 and the fees $0.40.
:::

:::check The book has 100 SPCX at every $0.05 from a best ask of $100.05. You buy 250 SPCX with a market order. What's your average price?
$100.09. You take 100 at $100.05, 100 at $100.10 and 50 at $100.15:
$25,022.50 ÷ 250.
:::

:::check You rest a bid at $99.95 instead of buying at the ask. What do you gain, and what do you risk?
You save the spread and the taker fee, but you only buy if a seller comes
down to $99.95. If SPCX rises instead, you never fill, and buy later at a
higher price.
:::

### Where your SPCX is

:::check You hold 10 SPCX and rest an ask for 4 of them on OpenBook. What's your position?
10 SPCX. The 4 are locked behind your ask, but still yours until it fills.
:::

:::check You have 6 SPCX in your wallet, 4 on OpenBook and 5 on Phoenix. Your mandate allows both venues with `max_position: 12`. Have you broken it?
Yes. A spot position is your holdings of the asset wherever they are:
6 + 4 + 5 = 15 SPCX, over 12 on both allowances.
:::

:::check Your ask fills on OpenBook as a maker, and nobody has consumed the event heap yet. Have you sold?
Yes. The fill happened, and an enforcer counts it from the venue's records:
the fill waits in the heap as an event. Your open orders account shows it
once the heap is consumed, or at once if the taker's transaction settled
it.
:::

### The mandate

:::check Your mandate is sell-only. You rest a bid to buy 1 SPCX back cheaply, and it never fills. Have you broken it?
Yes. The sides rule covers open orders as well as fills.
:::

:::check Your `min_sell_price` is $94. A market sell of 3,000 SPCX averages $95.20, but its last SPCX sells at $93.90. Have you broken it?
Yes. The limit applies to every fill, not the average.
:::

:::check Why does a mandate to sell 30,000 SOL usually have no drawdown limit?
Because the holding's value falls with SPCX whatever the agent does. A
drawdown limit would measure the market, not the agent.
:::

## Glossary

Every term this guide uses is in the [glossary](../glossary.md). In the text,
an underlined term shows its definition when you hover it or tap it.

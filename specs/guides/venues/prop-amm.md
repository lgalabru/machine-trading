# Guide: prop AMMs

Status: guide

This guide explains prop AMMs to someone who has only traded on an order
book: who is on the other side, where the price comes from, what a big trade
costs, and the ways a trade can fail that an order book doesn't have. A prop
AMM is not a new instrument. What you trade on it is [spot](../instruments/spot.md),
and the [spot profile](../../instruments/spot.md) is the normative text for
what you hold afterwards. Two venue profiles,
[BisonFi](../../venues/bisonfi.md) and [HumidiFi](../../venues/humidifi.md),
say how an enforcer reads those venues. Where this guide and a profile
disagree, the profile is right.

It builds on the [spot guide](../instruments/spot.md)'s example: you have
**$1,000**, and SPCX trades at **$100**. The `SPCX-USDC` market is
illustrative; the prop AMMs running on Solana today trade pairs such as
SOL-USDC, and work the same way. The figures are a teaching model with
made-up numbers. What the guide says about two real venues, BisonFi and
HumidiFi, comes from the documentation of [Surfpool](https://github.com/solana-foundation/surfpool)'s
scenario templates for them, which were still in review when this was
written.

:::glance
- **Who you trade with:** one trading firm, selling and buying from its own tokens
- **The price:** whatever the firm says, a little either side of what it thinks SPCX is worth
- **You can:** swap now, at its price, or not trade
- **You can't:** rest an order, or wait for a better price on the venue
- **The price moves when:** the firm changes its mind, not when you trade
- **Worst case:** the firm stops quoting, and your swap does nothing
:::

:::eli5
A man at the school gate trades football cards. He has a box of cards and a
pocket of coins, and a little board that says what he'll pay for a card and
what he'll sell one for. He rubs the board out and writes it again all day,
whenever he hears what cards are going for.

You can take his price or walk away. You can't leave a note saying "I'll pay
5 cents less" and come back later. If you want fifty cards at once, he asks
a bit more for each, and if his box is nearly empty he asks a lot more. If
he stops writing on the board for a while, he stops trading too: he won't
sell at a price he wrote this morning.

The man is the firm, the board is its quote, the box and the pocket are its
inventory, and the old board is a stale quote.
:::

## What a prop AMM is

On an order book, the spot guide's venue, anyone can rest an order, and you
trade with whoever rested the best one. On a **prop AMM**, a
proprietary AMM, there is one seller and one buyer: a trading firm, which
keeps its own tokens in a program on chain and trades them with whoever
asks.

The firm decides what SPCX is worth, its **fair value**, and writes that
number into the program. The program sells a little above it and buys a
little below it. The firm rewrites the number as often as every **slot**,
Solana's unit of time, about 0.4 seconds.

You trade with a **swap**: one transaction that sends your USDC and receives
SPCX, or the other way round. It fills now, at the firm's price, or it
doesn't fill. There is no limit order, nothing rests, and there is nothing
to cancel.

| | Order book | AMM | Prop AMM |
| --- | --- | --- | --- |
| **Whose tokens you trade against** | Other traders' orders | A pool anyone can add to | One firm's |
| **Who sets the price** | Whoever rests the best order | A formula, from what the pool holds | The firm, by publishing it |
| **The price moves when** | Orders are placed, filled or cancelled | Someone trades | The firm republishes |
| **You can** | Take, or rest an order | Swap | Swap |

:::history Why "prop AMM"
An **AMM**, an automated market maker, is a pool of two tokens that trades
at a price a formula works out from how much of each it holds. Uniswap made
the idea common after it launched in 2018. The pool's price only moves when
someone trades against it, so it is always a little behind, and the traders
who correct it are paid by the people who put tokens in the pool.

Trading firms on Solana took the shape and replaced the formula. To an
aggregator, their program looks like one more pool to swap against, which is
perhaps why the name stuck; but the tokens are the firm's own, *proprietary*,
and the price is the firm's too. By 2025 programs like these were
handling a large share of the SOL-USDC trading that aggregators route.
:::

### Your first trade

You swap your $1,000 for SPCX. The firm's **spread** is 4 bp: it sells 2 bp
above its fair value and buys 2 bp below it. The trade comes down to five
numbers:

| Number | Your trade | What it is |
| --- | --- | --- |
| **Fair value** | $100.00 | What the firm thinks SPCX is worth. The firm sets it. |
| **Spread, each side** | 2 bp | How far from fair value the firm trades. The firm sets it. |
| **The quote** | $100.02 | What you pay for one SPCX: $100 × 1.0002. |
| **You spend** | $1,000 | Your USDC, all of it. |
| **Position** | 9.998 SPCX | What you hold: $1,000 ÷ $100.02. |

Your SPCX are worth $999.80 at fair value, so the trade cost you **$0.20**.
The same $1,000 on the spot guide's book cost $0.70: half its $0.10 spread,
and a 2 bp fee. This model charges no separate fee; the firm's whole charge
is in its quote. A firm with nobody resting orders ahead of it, which can
change its price every slot, can afford to quote tighter than a book.

### As SPCX moves

On the book, the price moved because someone traded. Here it moves because
the firm says so. Nobody has to trade for the quote to change, and your own
swap doesn't move fair value at all.

| Fair value | The firm sells at: fair × 1.0002 | The firm buys at: fair ÷ 1.0002 |
| --- | --- | --- |
| $110 | $110.02 | $109.98 |
| $100 | $100.02 | $99.98 |
| $90 | $90.02 | $89.98 |

The firm also chooses the spread, and changes it. When SPCX is calm it
quotes tight. When SPCX is jumping about it quotes wide, because a wide
quote is how it avoids trading at a price that's already wrong.

```
you pay      = fair value × (1 + spread)        $100 × 1.0002 = $100.02
you receive  = fair value ÷ (1 + spread)        $100 ÷ 1.0002 = $99.98
```

```lab
{
  "model": "propamm.quote",
  "title": "What your trade costs, by the firm's spread",
  "lede": "Fair value is $100. Move the firm's spread and see what swapping your $1,000 costs, against the same trade on the spot guide's book.",
  "controls": [
    { "id": "side", "label": "You", "type": "choice", "options": [["buy", "Buy with $1,000"], ["sell", "Sell 10 SPCX"]], "value": "buy" },
    { "id": "spread", "label": "The firm's spread, each side", "min": 0.5, "max": 100, "step": 0.5, "value": 2, "format": "bp" }
  ],
  "fixed": { "deposit": 1000 },
  "presets": [
    { "label": "Your $1,000", "set": {}, "note": "At 2 bp you pay $100.02 and hold 9.998 SPCX. The trade cost $0.20, against $0.70 on the spot guide's book." },
    { "label": "As dear as the book", "set": { "spread": 7 }, "note": "At 7 bp the swap costs the same $0.70 as the book: its 5 bp of half-spread and 2 bp of fee." },
    { "label": "A nervous firm", "set": { "spread": 50 }, "note": "At 50 bp you pay $100.50 and the trade costs $4.98. The firm would rather not trade than trade wrong." },
    { "label": "Selling 10 SPCX", "set": { "side": "sell" }, "note": "Selling is the mirror image: 10 SPCX at $99.98 pay $999.80, and cost the same $0.20." }
  ],
  "caption": "The cost of a small swap is the spread and nothing else, and the firm decides it. It is cheaper than the book while the firm is confident, and dearer as soon as it isn't."
}
```

## A bigger trade

On the book, a big order walked up the asks, and each level cost more than
the last. A prop AMM gives a swap **one price for all of it**, and makes
that price worse for bigger trades in two ways.

The first is a **tier**: a wider spread for a bigger trade. In this model,
a trade up to $10,000 pays the 2 bp, a trade up to $100,000 pays 3 bp more,
and a larger one 10 bp more. The tier applies to the whole trade, so buying
101 SPCX costs more than twice what 100 does.

The second is the firm's **inventory**: the tokens it holds to pay you
with. It pays your SPCX out of its SPCX **vault**, and a seller's dollars
out of its USDC vault. The more of a vault your trade would take, the worse
the price, and a trade for more than the vault holds doesn't fill at all.
In this model the firm holds 20,000 SPCX and $2,000,000.

You buy 250 SPCX. The ticket gains two numbers, the tier and the skew:

| Number | Your trade | What it is |
| --- | --- | --- |
| **Fair value** | $100.00 | What the firm thinks SPCX is worth. |
| **Position** | 250 SPCX | What you want to hold. |
| **Notional** | $25,000 | 250 × $100: it picks the tier. |
| **Spread, each side** | 5 bp | The 2 bp, plus 3 bp for a trade over $10,000. |
| **Inventory skew** | 0.63 bp | You take 1.25% of the firm's 20,000 SPCX. |
| **The quote** | $100.06 | $100 × 1.0005 × 1.000063, for every one of the 250. |
| **You spend** | $25,014 | 250 × the quote. |

And for other sizes, against the spot guide's book with its fee:

| You buy | Spread | Inventory skew | The quote | Cost against fair value | The same on the book |
| --- | --- | --- | --- | --- | --- |
| 10 SPCX | 2 bp | 0.03 bp | $100.02 | $0.20 | $0.70 |
| 100 SPCX | 2 bp | 0.25 bp | $100.02 | $2.25 | $7.00 |
| 250 SPCX | 5 bp | 0.63 bp | $100.06 | $14.08 | $27.50 |
| 1,000 SPCX | 5 bp | 2.63 bp | $100.08 | $76.33 | $295.06 |
| 3,000 SPCX | 12 bp | 8.82 bp | $100.21 | $625.02 | $2,385 |

```
spread  = the firm's spread + the tier for the trade's notional
skew    = 0.5% × used ÷ (1 − used)      used: the share of the vault the trade takes
quote   = fair value × (1 + spread) × (1 + skew)      to buy; divide instead to sell
```

The skew's 0.5% is this model's number. A real firm has its own way of
leaning on its inventory, doesn't publish it, and can change it.

```lab
{
  "model": "propamm.size",
  "title": "What a bigger trade costs",
  "lede": "Choose how much to trade and how much the firm holds. The blue line is the prop AMM's one price for the whole trade; the dashed one is the average price on the spot guide's book, fee included.",
  "controls": [
    { "id": "side", "label": "You", "type": "choice", "options": [["buy", "Buy"], ["sell", "Sell"]], "value": "buy" },
    { "id": "size", "label": "SPCX", "min": 1, "max": 3000, "step": 1, "value": 10, "format": "num" },
    { "id": "inventory", "label": "The firm holds (SPCX, or their worth in USDC)", "min": 500, "max": 50000, "step": 500, "value": 20000, "format": "num", "hint": "What it can pay you out of." },
    { "id": "spread", "label": "The firm's spread, each side", "min": 0.5, "max": 100, "step": 0.5, "value": 2, "format": "bp" }
  ],
  "presets": [
    { "label": "Your 10 SPCX", "set": {}, "note": "10 SPCX take 0.05% of the firm's SPCX: $100.02 each, and $0.20 in all." },
    { "label": "Just over a tier", "set": { "size": 101 }, "note": "101 SPCX are a $10,100 trade, so all of them pay 5 bp: $5.31, where 100 SPCX cost $2.25." },
    { "label": "250 SPCX", "set": { "size": 250 }, "note": "250 SPCX cost $100.06 each, $14.08 in all. The book charged $27.50." },
    { "label": "3,000 SPCX", "set": { "size": 3000 }, "note": "3,000 SPCX take 15% of the firm's SPCX: 12 bp of spread and 8.82 bp of skew, $100.21 each. The book's average was $100.80." },
    { "label": "A thin vault", "set": { "size": 3000, "inventory": 4000 }, "note": "The same 3,000 SPCX from a firm holding 4,000: the skew is 150 bp, the price $101.62, and the book is now the cheaper place." },
    { "label": "More than it holds", "set": { "size": 3000, "inventory": 2000 }, "note": "The firm holds 2,000 SPCX. A swap for 3,000 doesn't fill at any price." }
  ],
  "caption": "A deep firm beats the book at every size here, because it gives one price instead of a ladder. What it holds is the limit: the price bends as a vault empties, and beyond the vault there is no trade."
}
```

## A quote that expires

A price the firm wrote a minute ago is dangerous to the firm. If SPCX has
fallen since, everyone would sell to it at the old, higher price. So the
program checks the quote's age before it trades: a quote more than a few
slots old is a **stale quote**, and the program won't fill against it.

The firm keeps its quote alive by republishing it, every slot or so. When
it stops, because its systems are down, or because the market is moving too
fast for it, the venue stops trading within about a second. An order book
doesn't do this: its resting orders stay until someone cancels them.

Say the firm last published in slot 1,000, and the program fills up to 2
slots later:

| Your swap lands in slot | The quote's age | What happens |
| --- | --- | --- |
| 1,000 | 0 slots | Fills at the quote. |
| 1,002 | 2 slots | Fills at the quote. |
| 1,003 | 3 slots | Stale: no fill. |

What "no fill" looks like depends on the venue, and the difference matters
to a bot:

- **The swap fails.** The transaction returns an error and nothing moves.
  You know, and can trade elsewhere.
- **The swap succeeds, and nothing moves.** The transaction is confirmed,
  you still have your USDC, and you have no SPCX. A bot that reads a
  confirmed swap as "I now hold SPCX" is wrong.

## Between the quote and the trade

You read the quote, build your swap and send it. It lands a slot or two
later, and the firm may have republished in between. You don't trade at the
price you read: you trade at the price the program holds when your swap
lands.

A swap protects you with a **minimum output**: the least SPCX it may pay
you. If the program would pay less, the swap fails and you keep your
dollars. You set it a little under what you expect, and how far under is
your tolerance.

You read $100.02, expect 9.998 SPCX for your $1,000, and allow 50 bp:

| Number | Your trade | What it is |
| --- | --- | --- |
| **The quote you read** | $100.02 | Fair value $100, plus 2 bp. |
| **You spend** | $1,000 | Your USDC. |
| **You expect** | 9.998 SPCX | $1,000 ÷ $100.02. |
| **Tolerance** | 50 bp | How much less you'd accept. You choose it. |
| **Minimum output** | 9.948 SPCX | 9.998 × (1 − 0.005). |

Then, depending on where fair value is when the swap lands:

| Fair value when it lands | The quote then | Your $1,000 would buy | What happens |
| --- | --- | --- | --- |
| $99.50 | $99.52 | 10.048 SPCX | Fills: you got more than you expected. |
| $100.00 | $100.02 | 9.998 SPCX | Fills, as expected. |
| $100.30 | $100.32 | 9.968 SPCX | Fills: 0.03 SPCX fewer, inside your tolerance. |
| $101.00 | $101.02 | 9.899 SPCX | Fails: under your 9.948 minimum. |

A tight tolerance fails more often; a loose one accepts worse prices. With
no minimum at all, the swap fills at whatever the firm says when it lands.

```
minimum output = what you expect × (1 − tolerance)      9.998 × 0.995 = 9.948 SPCX
the swap fills if   your dollars ÷ the quote when it lands ≥ minimum output
```

```lab
{
  "model": "propamm.landing",
  "title": "What happens when your swap lands",
  "lede": "You read $100.02 and send $1,000. Choose your tolerance, where the firm's fair value is when the swap lands, and how old its quote is by then.",
  "controls": [
    { "id": "tolerance", "label": "Your tolerance", "min": 0, "max": 200, "step": 5, "value": 50, "format": "bp", "hint": "How far under what you expect your minimum output sits." },
    { "id": "landing", "label": "Fair value when it lands", "min": 95, "max": 105, "step": 0.1, "value": 100, "format": "price" },
    { "id": "age", "label": "The quote's age, in slots", "min": 0, "max": 6, "step": 1, "value": 0, "format": "num", "hint": "This venue fills up to 2." },
    { "id": "stale", "label": "On a stale quote, the swap", "type": "choice", "options": [["revert", "Fails"], ["zero", "Succeeds, with nothing"]], "value": "revert" }
  ],
  "fixed": { "deposit": 1000, "spread": 2, "limit": 2 },
  "presets": [
    { "label": "Nothing moved", "set": {}, "note": "The quote is the one you read. You get the 9.998 SPCX you expected." },
    { "label": "SPCX up 0.3%", "set": { "landing": 100.3 }, "note": "The firm repriced to $100.32 before you landed. You get 9.968 SPCX, 0.03 fewer than expected, inside your 50 bp." },
    { "label": "SPCX up 1%", "set": { "landing": 101 }, "note": "At $101.02 your $1,000 buys 9.899 SPCX, under your 9.948 minimum. The swap fails and you keep your dollars." },
    { "label": "A loose tolerance", "set": { "tolerance": 200, "landing": 101.5 }, "note": "With 200 bp of tolerance the swap fills at $101.52: 9.85 SPCX, $15 worse than you expected." },
    { "label": "The firm went quiet", "set": { "age": 3 }, "note": "The quote is 3 slots old. This venue fails the swap: you keep your $1,000, and you know it." },
    { "label": "Quiet, and it says success", "set": { "age": 3, "stale": "zero" }, "note": "The same stale quote on a venue that confirms the swap and moves nothing. Your minimum output isn't checked: you have no SPCX and no error." }
  ],
  "caption": "The minimum output turns a bad price into a failed swap, which costs you nothing but the try. It can't help with a venue that reports success and does nothing: for that, read what you hold after the swap."
}
```

## Two prop AMMs

Two of the prop AMMs on Solana, as Surfpool's templates describe them. Both
are firms' private programs: neither publishes its account layout, and
either can change. Treat this as a snapshot, not a specification.

| | BisonFi | HumidiFi |
| --- | --- | --- |
| **The price** | A fair value the firm publishes | A fair value the firm publishes |
| **The spread** | A ladder of levels each side of fair value, with a small size at fair value itself that pays no spread | One spread, plus one of three tiers by the trade's size in the quote asset |
| **What limits size** | Depth figures the firm writes in the market's account | The balances of the vaults themselves: the quote shrinks fast as a vault thins |
| **A quote lasts** | Two slots | A limit stored in each market: 2 slots on SOL-USDC |
| **On a stale quote, a swap** | Succeeds and moves nothing; the minimum output isn't checked | Fails, with an error |
| **Also** | Each swap uses up the depth it trades through, so a second swap in the same slot fills worse; the next slot's first swap refills it | The firm republishes its spread along with its price |

## Parameters

What a swap on a prop AMM depends on, and who decides each:

| Parameter | Who sets it | What it means |
| --- | --- | --- |
| Fair value | The firm, every slot or so | What it thinks the base asset is worth. Its quotes sit either side. |
| Spread, and tiers | The firm | How far from fair value it trades, and how much further for bigger trades. |
| Inventory | The firm, and everyone who trades with it | What its vaults hold. Each swap takes from one vault and adds to the other. |
| How long a quote lasts | The venue's program | The age, in slots, after which it won't fill. |
| What a stale swap does | The venue's program | Fail, or succeed and move nothing. |
| Size | You | How much to swap. It picks the tier, and the share of the vault you take. |
| Minimum output | You | The least you'd accept. Below it, the swap fails. |
| Instrument reference, `max_position`, `sides`, limit prices | The mandate | As for any [spot](../instruments/spot.md#parameters) instrument: `bisonfi:spot:SOL-USDC`, or a market's address. |
| Markets, what a fill is, reference prices | The venue profile | Which account a reference names, how a swap is read as a fill, and the price feed holdings are valued at. |

## Risks

:::group
::risk The firm knows more than you
The firm sets its price from every market it watches, faster than you can.
When its quote looks cheap against another venue, ask whether it is a gift
or whether the other venue is the one that's late.
::
::risk The quote moves before you land
The price you trade at is the one in the program when your swap lands, not
the one you read. A minimum output bounds the damage.
::
::risk The firm goes quiet
A firm that stops publishing stops trading, within a few slots, and usually
when the market is at its worst. A bot with nowhere else to trade is stuck.
::
::risk Success that did nothing
On some venues a swap against a stale quote is confirmed and moves no
tokens. Check what you hold after a swap, not whether it was confirmed.
::
::risk An empty vault
The firm pays from what it holds. After a run of sellers it has few dollars
left, quotes sellers worse, and then can't pay them at all.
::
::risk A program you can't read
The firm's program is private and upgradeable. Its rules can change between
one trade and the next, and nothing is published first.
::
:::

## When to use it, and when not to

:::group
::use
- You want to **trade now**, a size the firm's inventory takes easily.
- The firm is quoting **tighter than the book**, fee included.
- You can **check both venues** and go where the price is better.
- You set a **minimum output** on every swap, and read your balances after.
::
::avoid
- You want to **rest an order** and be paid the spread: you can only take.
- The market is **moving fast**: the firm widens or stops, right then.
- Your trade is **large next to the firm's vault**.
- You have **nowhere else to trade** if the firm goes quiet.
::
:::

## Prop AMMs under a mandate

A [mandate](../../mandate.md) names its instruments as `<venue>:<kind>:<symbol>`.
A prop AMM needs no new kind: the instrument is `spot`, and the venue is one
of the profiles, such as `bisonfi:spot:SOL-USDC`. The profile says where the
account's tokens are and how a swap is read as a fill. No benchmark task
enforces either profile yet.

The spot rules apply as they do on a book, with four things worth knowing:

- **The universe covers swaps.** A swap leaves no order and no balance on
  the venue, but it is a fill, and a fill on a market the mandate doesn't
  list breaks the universe rule. That includes one hop of an aggregator's
  route: if the route passes through a prop AMM outside your universe, you
  traded there.
- **The position is in your wallet.** There are no resting orders and no
  balance left on the venue, so the base asset you hold is the base asset in
  your wallet. It is still the same position as on any other spot venue: 10
  SPCX bought on a prop AMM and 5 on OpenBook are 15 SPCX against every
  `max_position` on SPCX.
- **`sides` only has fills to check.** With nothing resting, a sell-only
  mandate can only be broken on a prop AMM by buying.
- **A swap is one fill.** `min_sell_price` and `max_buy_price` limit every
  fill. A swap of 3,000 SPCX is one fill at one price, where a market order
  on the book is many fills, and its last one is its worst.

Your 10 SPCX under a `min_sell_price` of $99.50: the firm buys at $99.98, so
the swap is allowed. If the firm widens to 60 bp it buys at $99.40, and the
same swap breaks the mandate. A minimum output of $995 for the 10 SPCX is the
floor written into the swap itself: under it, the swap fails instead.

## Scenarios

You hold SPCX and sell it under a `min_sell_price`. You can swap on the prop
AMM or send a market order to the spot guide's book. The lab plays the sale
through four states of the firm. Both venues are in the mandate's universe,
with the same floor.

```lab
{
  "model": "propamm.scenarios",
  "title": "Selling SPCX: the prop AMM or the book",
  "lede": "Choose what the firm is doing, how much you sell at once, and the mandate's floor. The blue line is the prop AMM's one price; the orange ones are the book's average and its last fill.",
  "controls": [
    { "id": "scenario", "label": "The firm", "type": "choice", "options": [["calm", "Calm"], ["wide", "Widens"], ["drained", "Low on dollars"], ["dark", "Goes quiet"]], "value": "calm" },
    { "id": "size", "label": "SPCX you sell at once", "min": 1, "max": 3000, "step": 1, "value": 10, "format": "num" },
    { "id": "floor", "label": "min_sell_price", "min": 95, "max": 100, "step": 0.05, "value": 99.5, "format": "price" }
  ],
  "presets": [
    { "label": "Your 10 SPCX", "set": {}, "note": "The firm buys at $99.98 and the book at $99.95, less its fee. The prop AMM is better, and well above the floor." },
    { "label": "The firm widens", "set": { "scenario": "wide" }, "note": "At 60 bp the firm buys at $99.40, under the $99.50 floor. The book still pays $99.95: sell there." },
    { "label": "The firm goes quiet", "set": { "scenario": "dark" }, "note": "No quote, so no swap. The book is the only venue that fills, at $99.95." },
    { "label": "Low on dollars", "set": { "scenario": "drained", "size": 250 }, "note": "250 SPCX would take most of the $30,000 the firm has left: it offers $97.51. The book pays $99.91 on average, and its last fill is $99.85." },
    { "label": "3,000 SPCX, calm", "set": { "size": 3000 }, "note": "One swap at $99.79 keeps the mandate. On the book the average is $99.23 and the last fill $98.50, which breaks it." },
    { "label": "3,000 SPCX, firm wide", "set": { "scenario": "wide", "size": 3000 }, "note": "The firm offers $99.22 and the book's last fill is $98.50. Neither takes 3,000 SPCX above $99.50 in one go: sell less at a time." }
  ],
  "caption": "The better venue changes with the firm's mood, and a floor that one venue respects the other can break. A bot that knows only one of them has no answer when the firm widens, runs low or goes quiet."
}
```

Selling a large holding in pieces, over time, is the subject of the
[selling a large position](../strategies/selling-a-large-position.md) guide.

## Check yourself

Three questions for each idea, all on SPCX. Work each one out on the ticket
first (fair value, spread, the quote, what you spend, position), then open
the answer.

### The quote

:::check Fair value is $100 and the firm's spread is 2 bp each side. You swap $1,000 for SPCX and swap it straight back. What did the round trip cost?
$0.40. You buy 9.998 SPCX at $100.02, and sell them at $99.98 for $999.60.
The same round trip on the spot guide's book cost $1.40.
:::

:::check Nobody has traded SPCX on the prop AMM for an hour, and its quote has fallen from $100.02 to $90.02. How?
The firm republished its fair value, from $100 to $90. On a prop AMM the
price moves when the firm changes it, not when someone trades.
:::

:::check The firm's quote is $100.02 and you'd like to buy at $99.90. What can you do on the prop AMM?
Nothing. You can swap at the firm's price or not trade: there are no resting
orders. To wait for $99.90 you'd rest a bid on an order book.
:::

### A bigger trade

:::check In this guide's model, 100 SPCX cost $2.25 against fair value and 101 SPCX cost $5.31. Why more than double?
The tier. 101 SPCX are a $10,100 trade, over $10,000, so every one of them
pays 5 bp of spread instead of 2 bp.
:::

:::check The firm holds 2,000 SPCX and you swap for 3,000. What price do you get?
None. The firm pays from its vault and can't pay what it doesn't hold: the
swap doesn't fill.
:::

:::check Sellers have been swapping SPCX for the firm's dollars all morning. What happens to the price the firm offers the next seller, with fair value unchanged?
It gets worse. The firm's USDC vault is emptier, so each sale takes a bigger
share of it, and the skew grows. The firm's price to buyers, paid from the
SPCX vault that just filled up, doesn't suffer.
:::

### Quotes that expire and move

:::check The firm last published in slot 1,000, and the program fills quotes up to 2 slots old. Your swap lands in slot 1,003. What happens?
It doesn't fill: the quote is 3 slots old, and stale. Depending on the
venue, the swap fails, or is confirmed and moves nothing.
:::

:::check You expect 9.998 SPCX for $1,000 and set a 50 bp tolerance. The firm reprices to $101.02 before your swap lands. What do you end up with?
Your $1,000. At $101.02 the swap would pay 9.899 SPCX, under your minimum
output of 9.948, so it fails.
:::

:::check Your swap was confirmed. Do you hold SPCX?
Not necessarily. On a venue that confirms a swap against a stale quote and
moves nothing, you hold what you held before. Read your balances.
:::

### The mandate

:::check You hold 6 SPCX in your wallet from a prop AMM and 4 locked behind an ask on OpenBook. What's your position?
10 SPCX. A spot position is your holdings of the base asset wherever they
are, and which venue you bought them on doesn't matter.
:::

:::check Your `min_sell_price` is $99.50. You swap 3,000 SPCX on the prop AMM at $99.79; on the book the same sale would average $99.23 with a last fill at $98.50. Which breaks the mandate?
The book. The limit applies to every fill: the swap is one fill at $99.79,
and the book's fills go down to $98.50.
:::

:::check Your universe is one order book, with a `min_sell_price` of $99.50. A prop AMM the mandate doesn't list is paying $99.98, more than the book. You swap your 10 SPCX there. Have you broken the mandate?
Yes. The price is above the floor, but the swap is a fill in an instrument
outside the universe. It breaks the universe rule even though it leaves no
position and no order on that venue.
:::

## Glossary

Every term this guide uses is in the [glossary](../glossary.md). In the text,
an underlined term shows its definition when you hover it or tap it.

# Guide: money markets (`money-market`)

Status: guide

This guide explains lending and borrowing on a money market to someone who
has never done either: what supplying earns, what borrowing costs, what
collateral is for, and how you get liquidated. It ends with how a
[mandate](../../mandate.md) bounds an agent that lends or borrows. The
[money-market profile](../../instruments/money-market.md) is the normative
text; when this guide and the profile disagree, the profile is right.

It builds one idea at a time, on the example the other guides use: you have
**$1,000**, and SPCX trades at **$100**. The SPCX reserve here is
illustrative: its limits are Kamino's SOL reserve's at the time of writing,
and the rate curve Kamino's USDC reserve's. The
[Kamino](../../venues/kamino.md) venue profile names the real reserves.

:::glance
- **Supply:** lend an asset to a pool, and earn interest on it
- **Borrow:** take an asset from the pool, and pay interest on it
- **To borrow you need:** collateral, worth more than what you borrow
- **The rates:** set by how much of the pool is lent out, and they move
- **Worst case:** your collateral falls, and the venue sells it to repay your debt
:::

:::eli5
A pawnbroker lends you money against something you leave with him, such as
a bike. He lends you less than the bike is worth, so that if you never come
back he can sell it and get his money back. If bikes suddenly got cheaper,
he'd sell yours early, before it was worth less than the loan.

A money market is a pawnbroker run by a program, with a twist: the money it
lends comes from everyone who deposited theirs, and they share the interest
you pay.

The bike is your collateral, what the pawnbroker lends is your borrow, and
selling the bike early is liquidation.
:::

## What a money market is

A money market is a set of **pools**, one per asset: a USDC pool, a SOL
pool, an SPCX pool. Anyone can **supply** an asset to its pool, and earn
interest on it. Anyone with enough **collateral** can **borrow** from a pool,
and pay interest. No one is matched with anyone: the pool lends to every
borrower at once, and every lender shares what they pay.

On Kamino, the pools are called **reserves**, and a set of reserves that back
each other is a **lending market**.

:::history Lombard loans
Lending against goods left as security is older than banking. The
merchants of Lombardy, in northern Italy, did so much of it across medieval
Europe that a loan against securities is still called a Lombard loan, and
London's financial street is Lombard Street, after them.
:::

### Supplying

You supply your $1,000 to the USDC pool.

| Number | Your supply | What it is |
| --- | --- | --- |
| **You supply** | $1,000 USDC | Lent to the pool. |
| **Position** | +1,000 USDC | Positive: the pool owes it to you. |
| **You receive** | collateral tokens | A claim on the pool, as Kamino records it. Their exchange rate rises as interest comes in. |
| **Supply rate** | about 3.5% a year | What lenders earn, today. It moves; see [below](#what-the-rates-depend-on). |

| After | Your supply is worth |
| --- | --- |
| 1 month | about $1,003 |
| 6 months | about $1,017 |
| 1 year | about $1,035 |

You can withdraw it whenever the pool has the USDC to give back. It
usually does, but not always: when nearly everything is lent out, you wait.

### Borrowing

Now say your $1,000 is in SPCX: you hold 10 SPCX, and want dollars without
selling them. You supply the 10 SPCX as collateral, and borrow USDC against
them.

| Number | Your borrow | What it is |
| --- | --- | --- |
| **Collateral** | 10 SPCX, $1,000 | Supplied to the SPCX pool, and held as security. |
| **Max LTV** | 74% | The most you may borrow, as a share of the collateral: $740. |
| **You borrow** | $500 USDC | From the USDC pool. |
| **LTV** | 50% | **Loan to value**: $500 ÷ $1,000. |
| **Liquidation LTV** | 80% | The LTV at which the venue may sell your collateral. |
| **Position** | SPCX +10, USDC −500 | Positive where you supply, negative where you borrow. |
| **Borrow rate** | about 5% a year | What borrowers pay, today. |

The buffer that matters is between your LTV and the liquidation LTV, not the
max LTV: the max LTV only stops you borrowing more.

### As SPCX moves

Your debt is in dollars, and your collateral in SPCX, so SPCX's price moves
your LTV:

| SPCX | Collateral | LTV: $500 ÷ collateral | Health: 80% ÷ LTV | |
| --- | --- | --- | --- | --- |
| $120 | $1,200 | 41.7% | 1.92 | safe |
| $100 | $1,000 | 50.0% | 1.60 | safe |
| $90 | $900 | 55.6% | 1.44 | safe |
| $75 | $750 | 66.7% | 1.20 | getting close |
| $62.50 | $625 | 80.0% | 1.00 | **liquidatable** |

**Health** is the liquidation LTV over your LTV: 1.60 means the debt could
grow, or the collateral fall, by a factor of 1.6 before liquidation. At 1,
the venue may liquidate.

```
LTV               = debt ÷ collateral value
health            = liquidation LTV ÷ LTV
liquidation price = debt ÷ (units of collateral × liquidation LTV)
                  = $500 ÷ (10 × 80%) = $62.50
```

```lab
{
  "model": "mm.borrow",
  "title": "Borrowing against your 10 SPCX",
  "lede": "Choose how much USDC to borrow, then move SPCX's price. The line is your LTV at every price.",
  "controls": [
    { "id": "borrow", "label": "You borrow", "min": 0, "max": 740, "step": 10, "value": 500, "format": "usd0", "hint": "Up to the 74% max LTV, at $100." },
    { "id": "price", "label": "SPCX moves to", "min": 40, "max": 140, "step": 0.5, "value": 100, "format": "price" },
    { "id": "months", "label": "Months of interest", "min": 0, "max": 36, "step": 1, "value": 0, "format": "num", "hint": "At 5% a year, with SPCX's price held." }
  ],
  "presets": [
    { "label": "Borrow $500", "set": {}, "note": "LTV 50%, health 1.60: SPCX can fall 37.5%, to $62.50, before you can be liquidated." },
    { "label": "Borrow the max", "set": { "borrow": 740 }, "note": "$740 is the 74% max LTV: health 1.08, and a fall of 7.5%, to $92.50, makes you liquidatable." },
    { "label": "SPCX falls 25%", "set": { "price": 75 }, "note": "At $75 your $500 is 66.7% of the collateral: health 1.20, and $12.50 more to fall." },
    { "label": "SPCX at $62.50", "set": { "price": 62.5 }, "note": "LTV 80%, health 1.00: liquidators may now repay part of your debt and take your SPCX." },
    { "label": "A year of interest", "set": { "months": 12 }, "note": "SPCX hasn't moved, but the debt is now $525 and the liquidation price $65.63: interest moves you toward the line too." },
    { "label": "Borrow $200", "set": { "borrow": 200 }, "note": "A small borrow: LTV 20%, health 4.00, and SPCX would have to fall to $25." }
  ],
  "caption": "Your LTV rises as SPCX falls, along a curve: the debt stays $500 while the collateral shrinks. The max LTV stops you borrowing more; the liquidation LTV is where the venue steps in."
}
```

## What the rates depend on

A pool's rates follow its **utilization**: how much of what's supplied is
lent out. Borrowers pay more as the pool runs low, which brings in lenders
and sends borrowers to repay. Lenders earn what borrowers pay, spread over
everything supplied, less the venue's share:

```
utilization = borrowed ÷ supplied
supply rate = borrow rate × utilization × (1 − the venue's share)
```

Kamino's USDC rates rise slowly up to a **kink**, then steeply: at 95%
utilization borrowers pay 5.51% a year, and at 100% they pay 36.34%. The
pool sits around 82% today. Kamino's app shows slightly different figures,
because it compounds the rates and measures utilization its own way; the
shape is the same.

```lab
{
  "model": "mm.rates",
  "title": "Rates on Kamino's USDC pool",
  "lede": "Move the pool's utilization. The orange line is what borrowers pay, the blue one what lenders earn.",
  "controls": [
    { "id": "utilization", "label": "Utilization (%)", "min": 0, "max": 100, "step": 0.5, "value": 82, "format": "num" }
  ],
  "presets": [
    { "label": "Today", "set": {}, "note": "At 82% utilization, borrowers pay 4.76% and lenders earn 3.51%: your $1,000 earns $35 a year." },
    { "label": "At the kink", "set": { "utilization": 95 }, "note": "At 95%, borrowers pay 5.51%, and only 5% of the pool is left for withdrawals." },
    { "label": "Past the kink", "set": { "utilization": 98 }, "note": "Three points past the kink, borrowers pay 24%: the steep part pulls them back to repay." },
    { "label": "Fully lent", "set": { "utilization": 100 }, "note": "At 100%, borrowers pay 36%, and lenders can't withdraw until someone repays." }
  ],
  "caption": "Below the kink, rates barely move. Above it, a few points of utilization multiply the borrow rate: the curve is built to keep some of the pool free."
}
```

Rates move every time anyone supplies, borrows, repays or withdraws. A
borrow that costs 5% today can cost 20% next week, and the debt grows at
whatever the rate is.

## Liquidation

Once your LTV reaches the liquidation LTV, anyone may **liquidate** you: repay
part of your debt, and take collateral worth a little more than what they
repaid. The extra is the **liquidation bonus**, which pays them for doing it.
On Kamino, each liquidation repays at most 20% of the debt, and the bonus
starts at the reserve's minimum (1%) and grows with how far past the line
you are, up to its maximum (10%).

At SPCX $62.50, your first trade is liquidatable:

| Step | SPCX | Debt | Your SPCX | Collateral | LTV |
| --- | --- | --- | --- | --- | --- |
| Before | $62.50 | $500 | 10.000 | $625.00 | 80.0% |
| A liquidator repays $100, takes $101 of SPCX (1% bonus) | $62.50 | $400 | 8.384 | $524.00 | 76.3% |
| SPCX falls to $55 | $55 | $400 | 8.384 | $461.12 | 86.7% |
| Another repays $80, takes $85.40 of SPCX (6.7% bonus) | $55 | $320 | 6.831 | $375.72 | 85.2% |

The first round brings your LTV back under 80%, and stops. If SPCX keeps
falling, more rounds follow, each taking a bonus. Unlike a perp's
liquidation, it's partial: you keep what's left, and you can stop it by
repaying or adding collateral. Past 95% LTV, Kamino can liquidate everything
at once.

## Borrowing to short

A money market is also how to bet on a fall with spot: supply USDC as
collateral, borrow SPCX, and sell it. If SPCX falls, you buy it back cheaper
and repay. It's a short like a perp's, with two differences: you pay the
SPCX pool's borrow rate instead of funding, and the liquidation is
partial instead of total.

## Parameters

| Parameter | Who sets it | What it means |
| --- | --- | --- |
| Instrument reference | The mandate | `<venue>:money-market:<symbol>`, such as `kamino:money-market:USDC`, a reserve the venue profile names. |
| `max_position`, `min_position` | The mandate | The most the account may supply, and the most it may borrow, as `min_position` below zero. `min_position: 0` forbids borrowing. |
| `min_health` | The mandate | The lowest health any borrow may reach. |
| Reserve and lending market | The venue profile | Which pool a symbol names, and which reserves back each other. |
| Max LTV, liquidation LTV | The venue | How much you may borrow, and where liquidation starts, per collateral asset. |
| Borrow factor | The venue | A weight on risky borrowed assets: 1.25 for SOL means $100 of SOL borrowed counts as $125 of debt. |
| Rate curve | The venue | The borrow rate at each utilization. |
| Liquidation bonus and close factor | The venue | What liquidators earn, and how much of the debt each liquidation repays. |

## Risks

:::group
::risk Liquidation
Your collateral falls, or your debt grows, until the venue may sell part of
your collateral, with a bonus to whoever does it.
::
::risk Rates
Borrow rates follow utilization, and can multiply in a day. Debt grows even
when prices don't move.
::
::risk Withdrawals
A fully lent pool has nothing to give back. Lenders wait until borrowers
repay.
::
::risk Prices
The venue values your collateral at its oracle's price. A wrong or delayed
price can liquidate a position that the market says is safe.
::
::risk Bad debt
If collateral falls faster than liquidators can act, a borrow can end up
worth more than its collateral, and lenders take the loss.
::
::risk The program
The pools are a program's accounts. Its code, its oracles and whoever can
upgrade it are part of the trade.
::
:::

## When to use it, and when not to

:::group
::use
- You hold an asset and want to **earn interest** on it.
- You want **dollars without selling** an asset you'd rather keep.
- You want to **short** with spot: borrow the asset and sell it.
- You can **watch your health**, and add collateral or repay in time.
::
::avoid
- Your collateral **moves a lot**, and you'd borrow close to the max LTV.
- You'd need to **withdraw at short notice**, from a pool that's nearly fully
  lent.
- You can't watch rates, and a **rate spike** would make the borrow too
  expensive.
- You don't trust the **oracle** the venue prices your collateral with.
::
:::

## Money markets under a mandate

The [mandate](../../mandate.md) covers money markets with three numbers:

- **The position** is what you've supplied minus what you've borrowed, in the
  reserve's asset: +1,000 USDC for your supply, −500 USDC for your borrow.
- **`max_position` and `min_position`** bound it. `min_position: 0` on a
  reserve means the agent may supply it, but never borrow it; a negative
  `min_position` is the largest borrow allowed.
- **`min_health`** keeps every borrow a margin above the venue's line: at
  `min_health: 1.25`, your $500 borrow breaks the mandate once SPCX falls
  below $78.13, long before the venue could liquidate it at $62.50.

:::example A borrow under a mandate
A mandate allowing `kamino:money-market:SPCX` with `min_position: 0`, and
`kamino:money-market:USDC` with `min_position: -500`, plus `min_health:
1.25`, lets the agent supply SPCX, borrow up to $500 against it, and never
let health fall below 1.25. The [mandate](../../mandate.md#examples) has the
same shape for SOL.
:::

## Check yourself

Three questions for each idea, all on SPCX at $100 and USDC. Work each one
out on the ticket first (collateral, debt, LTV, liquidation LTV), then open
the answer.

### Supplying and borrowing

:::check You supply $1,000 USDC at 3.5% a year. What's your position, and what's it worth after a year?
+1,000 USDC to start, and about $1,035 after a year, as interest raises the
value of your claim on the pool.
:::

:::check You supply 10 SPCX at $100, with a 74% max LTV. How much USDC can you borrow?
$740: 74% of $1,000.
:::

:::check You borrow $500 USDC against your 10 SPCX. What are your positions in the two reserves?
SPCX +10, USDC −500: positive where you supply, negative where you borrow.
:::

### LTV and health

:::check You borrowed $500 against 10 SPCX, with an 80% liquidation LTV. SPCX falls to $80. What are your LTV and health?
LTV 62.5% ($500 ÷ $800), and health 1.28 (80% ÷ 62.5%).
:::

:::check At what SPCX price can you be liquidated?
$62.50: $500 ÷ (10 × 80%). That's where your LTV reaches 80%.
:::

:::check SPCX doesn't move for a year, and you pay 5% on $500. How does your liquidation price change?
It rises to about $65.63: the debt is now $525, and $525 ÷ 8 = $65.63.
Interest moves you toward the line too.
:::

### Rates and liquidation

:::check Kamino's USDC pool is 95% lent out. What do borrowers pay, and what's the risk for lenders?
5.51% a year, at the kink. Only 5% of the pool is left, so a lender who
wants to withdraw more than that has to wait for borrowers to repay.
:::

:::check Your $500 borrow is liquidatable at SPCX $62.50. A liquidator repays 20% of it with a 1% bonus. What do they take?
$101 of SPCX, 1.616 SPCX at $62.50, for the $100 they repay. Your debt
falls to $400, and your LTV to 76.3%.
:::

:::check How is a money-market liquidation different from a perp's?
It's partial: each round repays at most 20% of the debt, you keep the rest
of your collateral, and you can stop it by repaying or adding collateral. A
perp's liquidation in the guide's model closes everything.
:::

### The mandate

:::check Your mandate allows `kamino:money-market:USDC` with `min_position: 0`. May the agent borrow USDC?
No. A position below zero is a borrow, and `min_position: 0` forbids it.
Supplying USDC is fine.
:::

:::check Your mandate sets `min_health: 1.25`. You borrowed $500 against 10 SPCX, with an 80% liquidation LTV. Below what SPCX price do you break it?
$78.13. Health is 0.8 × 10 × SPCX ÷ $500, which is 1.25 at SPCX = $78.125.
:::

:::check Why have `min_health` at all, when the venue liquidates at health 1 anyway?
Because liquidation costs a bonus every round, and a fast fall can skip past
the line. `min_health` makes the agent act, by repaying or adding
collateral, while there's still room.
:::

## Glossary

Every term this guide uses is in the [glossary](../glossary.md). In the text,
an underlined term shows its definition when you hover it or tap it.

# Glossary

Status: guide

The words the guides use, grouped by topic. In every guide, a word defined
here is underlined: hover it, or tap it, to read its definition. The first
paragraph of each entry is what the hover shows.

Where a mandate defines a term, the [mandate](../mandate.md) is
normative and this page only explains it.

## Positions

### Long

Also: longs, going long, go long, long position, long positions, is long, be long, are long, you're long
Not: as long as, how long, long time, long run, as long

A position that gains when the price rises and loses when it falls. You are
long when you own something, or hold a contract that behaves as if you did.
In the specs a long position is a positive quantity.

:::history Why "long"
By the mid-1800s American traders spoke of "the longs and the shorts" in the
grain markets. A trader *long* of wheat held plenty of it, and a trader
*short* of it had sold more than he held. The words describe how much you
have, not which way you bet, and have stuck ever since.
:::

### Short

Also: shorts, going short, go short, short position, short positions, shorting, short selling, short seller, is short, be short, are short, you're short
Not: short squeeze, in short, short of, short time

A position that gains when the price falls and loses when it rises. On a
perp you go short by selling a contract you don't hold. In the specs a short
position is a negative quantity.

A short's loss has no ceiling, because a price can rise without limit, while
a long can lose at most what the price was.

:::history The first famous short seller
In 1609 Isaac Le Maire, a merchant who had fallen out with the Dutch East
India Company, organised the first recorded bear raid: he and his partners
sold the company's shares for future delivery, expecting to buy them back
cheaper. The Dutch authorities banned the practice the following year, the
first of many bans on short selling.
:::

### Position

Also: positions
Not: in a position, position cap

What an account holds in an instrument: a signed quantity, positive when
[long](#long) and negative when [short](#short). The
[instrument profile](../instruments/perp.md) defines how it is measured.

### Underlying

Also: the underlying's, underlyings

What a contract's price follows: a token such as SOL, one share of a
company such as SpaceX, a stock index such as the S&P 500, a commodity such as
gold, or even a private company's valuation. The perp tracks it without
anyone holding it.

### Base units

Also: base unit

What a position is counted in: shares or tokens, not dollars. A position of
70 SPCX is 70 base units, whatever SPCX is worth. The mandate's
`max_position` is in base units.

### Lot size

Also: lot sizes

The smallest step a venue lets a position change by. A venue profile says
how to convert lots to base units.

### Notional

Also: notional value

A position's size in dollars: its quantity times the price. A position of 50
units at $100 has a notional of $5,000, however little margin backs it.

:::history Why "notional"
The word comes from swaps, where two banks exchange the interest on a
principal that never changes hands: it exists only *notionally*. Perps
borrowed it for the same reason. Nobody pays the notional, but every
payment is computed from it.
:::

### Entry price

Also: entry prices

The average price a position was opened at. PnL is measured from it.

### PnL

Also: profit and loss

Profit and loss: what a position has made or lost. Unrealized PnL is what it
would make if closed at the current price; realized PnL is what closed
trades actually made.

### Base and quote

Also: base asset, quote asset, base token, quote token, pair, trading pair

The two assets of a spot market, written base-quote: in `SOL-USDC`, SOL is
the base, what the market trades, and USDC the quote, what prices are in.

### Wallet

Also: wallets

The agent authority's own token accounts, outside any venue. Tokens move
between the wallet and a venue's accounts as the agent deposits, trades and
settles.

### Wrapped SOL

Also: wSOL

SOL in the form of a token, which venues can trade like any other.
Native SOL also pays transaction fees, so the venue profiles count only
the wrapped kind as a holding.

### Hedge

Also: hedges, hedging, hedged
Not: hedge fund, hedge funds

A position held to offset the risk of another. A short perp hedges a
holding of the same token: what one loses, the other gains.

:::history Why "hedge"
A hedge was a fence of bushes around a field. By the 1600s, "to hedge a
bet" meant to bet on the other side as well, fencing in what you could lose.
:::

## Prices

### Spot price

Also: spot, spot prices

The price for buying or selling the asset itself, now, as opposed to a
contract on it.

### Mark price

Also: the mark, mark prices

The price a venue values positions at, and liquidates them at. It is usually
built from the [index price](#index-price) rather than the venue's last trade,
so a single odd trade can't liquidate anyone. The venue profile defines it.

### Index price

Also: index prices

The underlying's price, combined from other markets. A perp's mark and
funding follow it.

### Market hours

Also: trading hours, market closed, closed market

The hours an underlying's own market trades. A perp trades around the clock,
so outside them the venue prices it on its own: on Phoenix, equity perps use
external prices 24 hours a day, 5 days a week, and their own order book,
within a cap, over weekends and holidays.

### Mid price

Also: mid, the mid

Halfway between the best bid and the best ask.

### Arrival price

Also: arrival

The price when a sale or a purchase starts, which its result is measured
against.

### Last price

Also: last trade

The price of the most recent trade on the venue. It can differ from the
[mark](#mark-price), especially in a thin market.

### Premium

Also: premiums

How far a perp's price is above its index, or below it when negative. A
premium makes [funding](#funding) positive, so longs pay shorts until it
closes.

### Bid and ask

Also: bid, bids, best bid, best ask

The highest price a buyer is offering (the bid) and the lowest a seller is
asking (the ask). A market order buys at the ask and sells at the bid.

### Spread

Also: bid-ask spread
Not: spreads your, spread over

The gap between the best ask and the best bid. Crossing it is a cost every
taker pays.

### Oracle

Also: oracles, price oracle

A service that publishes prices from outside a blockchain onto it, so a
program can use them. On-chain venues build their mark and index from
oracles, and a wrong oracle price can liquidate positions.

## Futures and perpetuals

### Future

Also: futures, dated future, dated futures, futures contract

A contract to buy or sell an asset at a set price on a set date, its expiry.
At expiry it settles at the underlying's price, which pulls its price toward
spot as the date nears.

:::history The first futures market
Japan's Dojima Rice Exchange in Osaka, officially recognised in 1730, is
usually counted as the first organised futures market. Samurai were paid in
rice, and traders bought and sold rice to be delivered later, with
standardised contracts and a clearing house, more than a century before
Chicago's grain futures.
:::

### Expiry

Also: expiry date, expires, expiration

The date a dated future settles. A perpetual has none.

### Perpetual future

Also: perpetual futures, perpetual, perpetuals, perp, perps, perpetual swap, perpetual swaps

A [future](#future) with no expiry, held near the underlying's price by
[funding](#funding) instead. It is the instrument kind `perp`; see the
[guide](instruments/perp.md).

:::history Where perps come from
The economist Robert Shiller proposed perpetual futures in 1993, as a way
to trade assets such as house prices that have no market to settle against.
They took off in crypto instead: BitMEX launched its perpetual swap on
bitcoin in 2016, and perps are now the most traded crypto derivative.
:::

### Funding

Also: funding payments, funding payment

Payments between longs and shorts that keep a perp's price near its index.
When the perp trades above the index, funding is positive and longs pay
shorts; below it, shorts pay longs. Funding is paid on the notional.

### Funding rate

Also: funding rates

The share of notional paid in funding each interval, in
[basis points](#basis-point) or percent. A rate of 1 bp an hour is about 88%
a year.

### Funding interval

Also: funding intervals

How often funding is paid: every hour, or every eight, depending on the
venue. The venue profile says.

## Margin and risk

### Margin

Not: a margin above, a margin of


The deposit that backs a position and absorbs its losses. Opening a perp
needs [initial margin](#initial-margin); keeping it open needs
[maintenance margin](#maintenance-margin).

### Initial margin

The margin a venue requires to open a position, as a share of its notional.
It sets the highest leverage you can open at.

### Maintenance margin



The least equity a position may have, as a share of its notional. Below it,
the venue liquidates the position. It is lower than the initial margin, so a
new position has some room to lose.

### Cross margin

Also: cross-margin

One pool of margin backs every position in the account, so a gain on one
supports a loss on another, and a big enough loss on one can liquidate them
all. On Phoenix, equity perps such as SPCX are cross-margined.

### Isolated margin

Each position has its own margin, and losing it liquidates only that
position. The guides' figures use isolated margin, since it is simpler to
show.

### Equity

Not: equity perp, equity perps, equity perpetuals
Margin plus unrealized PnL: what the account would be worth if every
position were closed at the mark.

### Leverage

Also: leveraged

Notional divided by margin. At 5× leverage, $1,000 of margin backs $5,000 of
position, and a 1% move is 5% of the margin.

### Liquidation

Also: liquidations, liquidated, liquidates, liquidate

The venue closing a position whose equity fell below maintenance margin, or,
on a money market, anyone repaying part of a borrow and taking collateral
for it once the borrow's LTV reaches the liquidation LTV. It is forced, and
what it takes doesn't come back if the price recovers.

### Liquidation price

Also: liquidation prices, liquidation level

The mark price at which a position would be liquidated. Higher leverage
moves it closer to the entry price.

### Insurance fund

A venue's reserve for losses a liquidation doesn't cover, when the price
gaps past the liquidation price before the position can be closed.

### Auto-deleveraging

Also: ADL

When the insurance fund can't cover a loss, some venues close part of the
most profitable opposing positions instead. A risk of winning on a perp
that a spot holder doesn't have.

### Volatility

Also: volatile

How much a price moves, usually measured as the typical size of its daily
moves. Over time, moves grow roughly with the square root of how long you
wait.

### Drawdown

Also: drawdowns

How far an account's value is below its running peak. The mandate's
`max_drawdown_usd` limits it.

### High-water mark

Also: running peak

The highest value an account has reached. A [drawdown](#drawdown) is
measured from it, not from where the account started.

:::history Why "high-water mark"
The line a flood leaves on a wall. Hedge funds borrowed it for their fees:
a manager earns a performance fee only on value above the previous
high-water mark, so a fund that falls and recovers doesn't pay twice.
:::

## Trading

### Order book

Also: order books

The list of buy and sell orders resting on a venue, by price. A thin book
has few orders near the price, so large orders move it.

### Limit order

Also: limit orders, resting order, resting orders

An order to trade at a set price or better. It rests on the book until it
fills or is cancelled.

### Market order

Also: market orders

An order to trade now at the best available price. It fills at once, at
whatever the book offers.

### Maker and taker

Also: maker, taker, makers, takers

A maker's order rests on the book and adds liquidity; a taker's fills
against it. Venues usually charge takers more, and sometimes pay makers.

:::history Paying people to wait
Island, an electronic exchange of the late 1990s, popularised paying a
rebate to orders that rested on its book and charging the orders that took
them. The model spread to nearly every electronic venue.
:::

### Open orders account

Also: open orders accounts, OpenOrders

On OpenBook, the account that holds a trader's orders and balances on one
market: what its fills paid it, and what its resting orders lock.

### Seat

Also: seats

On a Phoenix spot market, a trader's place in the market account, holding
its free and locked balances.

### Settle

Also: settles, settled, settling, settle funds

Moving what a venue holds for you back to your wallet. Unsettled balances
are still yours, and still count as your holdings.

### Slippage

The difference between the price you expected and the price you got,
because your order moved through the book or the price moved before it
filled.

### Liquidity

Also: illiquid, thin market, thin book

How much can be traded near the current price without moving it. A thin
market has little.

### Basis point

Also: basis points, bp

One hundredth of a percent: 0.01%. 100 bp is 1%.

### Bull and bear

Also: bull market, bear market, bullish, bearish, bulls, bears

A bull expects prices to rise, a bear expects them to fall. A bull market
rises for a long time, and a bear market falls.

:::history Why bears
In 18th-century London, a "bearskin jobber" sold bearskins he didn't yet
have, after the proverb about selling the bear's skin before catching the
bear: a short seller. The bull, its opposite, followed, perhaps because a
bull tosses its horns up while a bear swipes down.
:::

### Flash crash

Also: flash crashes

A sharp fall in price within minutes, followed by a fast recovery. The
benchmark's `phoenix-flash-crash` task replays such sessions.

:::history The Flash Crash
On 6 May 2010 the Dow Jones fell nearly 1,000 points, about 9%, in minutes,
and recovered most of it within half an hour. Some shares briefly traded at
a cent. The episode gave the phenomenon its name.
:::

### Short squeeze

Also: short squeezes

A fast rise that forces shorts to buy back, which pushes the price higher
and forces more of them out.

:::history Squeezes
In October 2008 Volkswagen briefly became the most valuable company in the
world when shorts found that Porsche controlled most of its shares and
scrambled to buy. In January 2021 GameStop's price rose more than tenfold
in weeks as retail traders squeezed the funds short of it.
:::

## Money markets

### Money market

Also: money markets, lending market, lending markets, lending protocol

A program that pools an asset from lenders and lends it to borrowers
against collateral. See the [guide](instruments/money-market.md).

### Reserve

Also: reserves, pool, pools

One asset's pool in a lending market: everyone's supply of it, and
everyone's borrows from it.

### Supply

Also: supplies, supplied, supplying, lend, lends, lending, lender, lenders

Lending an asset to a money market's pool, to earn interest on it.

### Borrow

Also: borrows, borrowed, borrowing, borrower, borrowers

Taking an asset from a money market's pool against collateral, and paying
interest on it until it's repaid.

### Collateral

Also: collateral token, collateral tokens

What a borrower has supplied and the venue holds as security. On Kamino,
supplying gives you collateral tokens, a claim on the pool that rises in
value as interest comes in.

### Obligation

Also: obligations

On Kamino, the account that holds one owner's deposits and borrows in a
lending market, and their values as of its last refresh.

### LTV

Also: loan to value, loan-to-value, max LTV, liquidation LTV, current LTV

Loan to value: debt divided by collateral value. Below the **max LTV** you
may borrow more; at the **liquidation LTV**, the venue may liquidate.

### Health

Also: health factor

The liquidation LTV divided by the current LTV. Above 1 a borrow is safe;
at 1 or below, the venue may liquidate it.

### Borrow factor

Also: borrow factors

A weight on risky borrowed assets: with a borrow factor of 1.25, $100
borrowed counts as $125 of debt in the LTV.

### Utilization

The share of a pool's supply that is lent out. Borrow rates rise with it,
slowly up to the curve's **kink**, steeply after.

### Liquidation bonus

Also: liquidation bonuses, liquidation penalty

The extra collateral a liquidator takes for repaying part of a borrow: what
the borrower pays for being liquidated.

### Close factor

The most of a borrow's debt one liquidation may repay: 20% on Kamino, unless
the borrow is near insolvency.

### Bad debt

A borrow worth more than its collateral: liquidation can no longer repay it,
and the pool's lenders take the loss.

## Strategies

### Strategy

Also: strategies

A repeatable rule for when to trade, which side to take, and how much. A
mandate bounds what a strategy may do, but never says what it is.

### Arbitrage

Also: arbitrageur, arbitrageurs, arbitrages, arb

Buying something where it is cheap and selling it where it is dear, at the
same time, so the profit doesn't depend on which way the price moves next.
See the [guide](strategies/arbitrage.md).

### Leg

Also: legs

One of the positions in a trade made of several. An arbitrage has two legs
whose price risks cancel.

### Delta-neutral

Also: delta neutral, hedged position

Holding positions whose gains and losses from a price move cancel, so the
total doesn't change when the price does.

### Cash and carry

Also: cash-and-carry, basis trade, carry trade, carry

Buying an asset and shorting a future or perp on it, to earn the premium or
the funding without exposure to the price.

### Market making

Also: market maker, market makers, making markets

Quoting both a price to buy and a price to sell, and earning the gap between
them from the traders who take the quotes. The market maker ends up holding
whatever those traders didn't want.

### Implementation shortfall

Also: shortfall, shortfalls

What a sale or purchase lost against its arrival price: the paper value
minus the proceeds and the value of whatever is left. See the
[guide](strategies/selling-a-large-position.md).

### Market impact

Also: impact, temporary impact, permanent impact, price impact

How far your own trading moves the price. Temporary impact fades as the
book refills; permanent impact stays, because the market has learned there
is a seller or a buyer.

### TWAP

Also: time-weighted average price

Selling, or buying, the same amount in every period of a session, so the
result is roughly the market's average price over it.

### Schedule

Also: schedules, slice, slices

How much a large order trades when: a slice in each period, sized by a
rule.

### Front-running

Also: front-runner, front-runners, front-run, traded ahead of

Trading just ahead of an order you've seen or predicted, to profit from
the price move it causes.

## Mandates

### Mandate

Also: mandates

A JSON document stating what an agent may trade on someone's behalf, and the
limits it must stay within. See the [mandate](../mandate.md).

### Principal

Also: principals

The person or fund whose capital the agent trades.

### Agent

Also: agents

The program that trades under a mandate, such as a bot an AI agent wrote.

### Universe

The instruments a mandate allows. Holding or ordering anything outside it
breaks the mandate.

### Position cap

Also: max_position, min_position

The range a mandate allows an instrument's position, in base units: up to
`max_position`, and down to `min_position`, which is `−max_position` unless
the mandate sets it. Open orders don't count until they fill.

### Minimum health

Also: min_health

The lowest health a mandate lets any of the account's borrows reach:
`min_health`. Set above 1, it makes the agent act before the venue could
liquidate it.

### Drawdown limit

Also: max_drawdown_usd

The most, in USD, an account's value may fall below its running peak:
`max_drawdown_usd`. It covers the whole account.

### Sides

Also: sell-only, buy-only

Whether a mandate lets the agent buy, sell, or both, in an instrument. It
covers resting orders as well as fills.

### Limit price

Also: price floor, floor, min_sell_price, max_buy_price

The worst price at which a mandate lets the agent sell (`min_sell_price`)
or buy (`max_buy_price`). It applies to every fill, not the average.

### Enforcer

Also: enforcers, grader

Whatever checks an agent against its mandate without trusting it. In the
benchmark, the grader.

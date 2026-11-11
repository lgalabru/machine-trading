# SPCX market maker

`run.sh` starts the prebuilt native executable in `bin/bot`. It needs no compiler,
SDK, Python, Phoenix metadata API, or writable application directory at runtime. The GCC
runtime library is bundled; the remaining dependencies are Debian's libc/libm.

The strategy anchors quotes to the oracle, steps inside the competing book, and
skews prices against inventory. It observes completed public Solana blocks for
SPCX taker orders, excluding its own transactions. A directional burst detector
can establish inventory in the flow's direction, while ordinary quoting uses a
smaller inventory limit. Positions unwind passively when signals expire.

Before quoting, it cancels existing orders, settles maker fills, and values
collateral plus the position at the oracle. Exposure is bounded by both the
mandate and a 5% adverse-gap allowance with $35 reserved for execution costs.
A $150 drawdown triggers cancellation and liquidation. 

Only standard Solana RPC methods are used. The bot reads no scenario files.
`mtx_kit::run` handles the turn protocol. Every order is for the configured SPCX
market. The turn loop never builds or downloads anything.

## Development

With the provided Rust toolchain and `/opt/mtx/sdk` available:

```sh
./build.sh
PATH=/usr/local/cargo/bin:$PATH cargo test --release
```

Builds install atomically. Set `MTX_BUILD_OUTPUT` to build a candidate elsewhere
without replacing the installed executable. An optional local `config.json`
overrides strategy parameters; absent that file, compiled defaults apply.

The shipped configuration uses a 12-share ordinary inventory limit and a
19.9-share burst target, always reduced by the mandate and drawdown reserve.
A regularized trend estimate combines oracle returns with aggregate taker flow.
After an observed jump, the ordinary drift target returns to zero.

## Validation

Four unit tests pass. On 72 separately generated scenarios, this exact binary
earned $3,336.49, with zero disqualifications and a maximum drawdown of
$91.82. The complete per-session summary and binary hash are
in `validation.json`. These are development scenarios, separate from the hidden
grading set.

"""Pays for a model call over x402 on Solana, for the key proxy: no key, a wallet.

A provider on x402 (BlockRun's Solana gateway, `sol.blockrun.ai`) takes no
API key. It answers a model call with HTTP 402 and a quote: the USDC it
wants, to whom, on which network, with a recent blockhash to sign on. The
caller signs a USDC transfer for that amount (an SPL TransferChecked, with
the gateway's facilitator as fee payer) and sends the call again with the
signed transaction in a `PAYMENT-SIGNATURE` header; the gateway settles it
and answers. The wallet's secret plays the part of the key: it stays in the
key proxy, on the bench machine, and a trial never holds it.

Standard library only, like the rest of the harness (`python3 -I`): base58,
Ed25519 (RFC 8032), the associated token account's program-derived address
and a v0 transaction, built the way the x402 SDK's `ExactSvmScheme` builds
them (x402 2.26; test_x402_solana.py checks the bytes against it). The
blockhash comes from the quote, so nothing here talks to a Solana RPC.

What a quote must be before it is signed (`choose`): the `exact` scheme on
the route's network, in an asset listed in ASSETS (USDC, whose token program
and decimals are fixed, so no mint lookup), a fee payer, a blockhash, and an
amount within the route's cap. The batch's budget (`Ledger`) bounds the sum.

Which refused payments may be signed again (`safe_to_resign`) follows the
BlockRun SDK: only a refusal the gateway proves came before any transfer was
broadcast (PAYMENT_UNDERPAID, PAYMENT_REPLAY, PAYMENT_INVALID and their
titles), never one in or after settlement, and never one a new signature
can't cure (no USDC, no token account).
"""

import base64
import binascii
import hashlib
import json
import os
import re
import threading
import time

# --- base58 -----------------------------------------------------------------

B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
B58_INDEX = {c: i for i, c in enumerate(B58)}


def b58encode(data: bytes) -> str:
    n = int.from_bytes(data, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = B58[r] + out
    return "1" * (len(data) - len(data.lstrip(b"\0"))) + out


def b58decode(text: str) -> bytes:
    n = 0
    for c in text:
        if c not in B58_INDEX:
            raise ValueError("not base58")
        n = n * 58 + B58_INDEX[c]
    body = n.to_bytes((n.bit_length() + 7) // 8, "big") if n else b""
    return b"\0" * (len(text) - len(text.lstrip("1"))) + body


def pubkey(text: object) -> bytes:
    """A Solana address, as its 32 bytes, or ValueError."""
    if not isinstance(text, str) or not 32 <= len(text) <= 44:
        raise ValueError("not an address")
    raw = b58decode(text)
    if len(raw) != 32:
        raise ValueError("not an address")
    return raw


# --- Ed25519 (RFC 8032) -------------------------------------------------------

P = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
D = -121665 * pow(121666, P - 2, P) % P
SQRT_M1 = pow(2, (P - 1) // 4, P)


def _inv(x: int) -> int:
    return pow(x, P - 2, P)


def _recover_x(y: int, sign: int) -> int | None:
    if y >= P:
        return None
    x2 = (y * y - 1) * _inv(D * y * y + 1) % P
    if x2 == 0:
        return None if sign else 0
    x = pow(x2, (P + 3) // 8, P)
    if (x * x - x2) % P:
        x = x * SQRT_M1 % P
    if (x * x - x2) % P:
        return None
    return P - x if (x & 1) != sign else x


_GY = 4 * _inv(5) % P
_GX = _recover_x(_GY, 0)
G = (_GX, _GY, 1, _GX * _GY % P)
IDENTITY = (0, 1, 1, 0)


def _add(p, q):
    a = (p[1] - p[0]) * (q[1] - q[0]) % P
    b = (p[1] + p[0]) * (q[1] + q[0]) % P
    c = 2 * p[3] * q[3] * D % P
    d = 2 * p[2] * q[2] % P
    e, f, g, h = b - a, d - c, d + c, b + a
    return (e * f % P, g * h % P, f * g % P, e * h % P)


def _mul(s: int, p):
    q = IDENTITY
    while s:
        if s & 1:
            q = _add(q, p)
        p = _add(p, p)
        s >>= 1
    return q


def _equal(p, q) -> bool:
    return (p[0] * q[2] - q[0] * p[2]) % P == 0 and (p[1] * q[2] - q[1] * p[2]) % P == 0


def _compress(p) -> bytes:
    z = _inv(p[2])
    x, y = p[0] * z % P, p[1] * z % P
    return (y | ((x & 1) << 255)).to_bytes(32, "little")


def _decompress(s: bytes):
    y = int.from_bytes(s, "little")
    sign, y = y >> 255, y & ((1 << 255) - 1)
    x = _recover_x(y, sign)
    return None if x is None else (x, y, 1, x * y % P)


def _hash_int(*parts: bytes) -> int:
    return int.from_bytes(hashlib.sha512(b"".join(parts)).digest(), "little") % L


def _expand(seed: bytes) -> tuple[int, bytes]:
    h = hashlib.sha512(seed).digest()
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 8
    a |= 1 << 254
    return a, h[32:]


def public_key(seed: bytes) -> bytes:
    return _compress(_mul(_expand(seed)[0], G))


def sign(seed: bytes, message: bytes) -> bytes:
    a, prefix = _expand(seed)
    public = _compress(_mul(a, G))
    r = _hash_int(prefix, message)
    big_r = _compress(_mul(r, G))
    s = (r + _hash_int(big_r, public, message) * a) % L
    return big_r + s.to_bytes(32, "little")


def verify(public: bytes, message: bytes, signature: bytes) -> bool:
    if len(public) != 32 or len(signature) != 64:
        return False
    a, r = _decompress(public), _decompress(signature[:32])
    s = int.from_bytes(signature[32:], "little")
    if a is None or r is None or s >= L:
        return False
    h = _hash_int(signature[:32], public, message)
    return _equal(_mul(s, G), _add(r, _mul(h, a)))


def on_curve(raw: bytes) -> bool:
    """Whether 32 bytes decompress to a point, as Solana's own check reads
    them (curve25519-dalek: the sign bit ignored, y taken modulo p): a
    program-derived address must not."""
    y = (int.from_bytes(raw, "little") & ((1 << 255) - 1)) % P
    x2 = (y * y - 1) * _inv(D * y * y + 1) % P
    return x2 == 0 or pow(x2, (P - 1) // 2, P) == 1


# --- Solana -------------------------------------------------------------------

SOLANA_MAINNET = "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"
SOLANA_DEVNET = "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1"
TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
ASSOCIATED_TOKEN_PROGRAM = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
COMPUTE_BUDGET_PROGRAM = "ComputeBudget111111111111111111111111111111"
MEMO_PROGRAM = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"
# What the x402 SDK's exact scheme sets (x402.mechanisms.svm.constants); the
# facilitator checks the price against its own ceiling.
COMPUTE_UNIT_LIMIT = 20_000
COMPUTE_UNIT_PRICE_MICROLAMPORTS = 1
MAX_MEMO_BYTES = 256
# The assets a quote may ask for: (network, mint) -> (token program, decimals).
# USDC only, so the mint's program and decimals need no RPC.
ASSETS = {
    (SOLANA_MAINNET, "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"): (TOKEN_PROGRAM, 6),
    (SOLANA_DEVNET, "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU"): (TOKEN_PROGRAM, 6),
}
NETWORKS = {"solana": SOLANA_MAINNET, "solana-devnet": SOLANA_DEVNET}


def program_address(seeds: list[bytes], program: bytes) -> bytes:
    """Solana's find_program_address: the first bump, from 255 down, whose
    hash is off the curve."""
    for bump in range(255, -1, -1):
        candidate = hashlib.sha256(b"".join(seeds) + bytes([bump]) + program + b"ProgramDerivedAddress").digest()
        if not on_curve(candidate):
            return candidate
    raise ValueError("no program address")


def token_account(owner: bytes, mint: bytes, token_program: bytes) -> bytes:
    """The owner's associated token account for the mint."""
    return program_address([owner, token_program, mint], pubkey(ASSOCIATED_TOKEN_PROGRAM))


def shortvec(n: int) -> bytes:
    """Solana's compact-u16 length."""
    out = bytearray()
    while True:
        byte, n = n & 0x7F, n >> 7
        if n:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def compile_v0(fee_payer: bytes, instructions: list, blockhash: bytes) -> bytes:
    """A v0 message with no lookup tables, its keys ordered as Solana's
    CompiledKeys orders them (solders' MessageV0.try_compile): the fee payer,
    then writable signers, read-only signers, writable and read-only others,
    each group by key. `instructions` are (program, [(key, signer, writable)],
    data). Returned with its 0x80 version prefix: the bytes that are signed."""
    meta: dict[bytes, list[bool]] = {fee_payer: [True, True]}
    for program, accounts, _ in instructions:
        meta.setdefault(program, [False, False])
        for key, signer, writable in accounts:
            m = meta.setdefault(key, [False, False])
            m[0] |= signer
            m[1] |= writable
    rest = sorted(k for k in meta if k != fee_payer)
    groups = [
        [k for k in rest if meta[k] == [True, True]],
        [k for k in rest if meta[k] == [True, False]],
        [k for k in rest if meta[k] == [False, True]],
        [k for k in rest if meta[k] == [False, False]],
    ]
    keys = [fee_payer] + [k for g in groups for k in g]
    index = {k: i for i, k in enumerate(keys)}
    out = bytearray([0x80, 1 + len(groups[0]) + len(groups[1]), len(groups[1]), len(groups[3])])
    out += shortvec(len(keys)) + b"".join(keys) + blockhash + shortvec(len(instructions))
    for program, accounts, data in instructions:
        out.append(index[program])
        out += shortvec(len(accounts)) + bytes(index[k] for k, _, _ in accounts)
        out += shortvec(len(data)) + data
    out += shortvec(0)
    return bytes(out)


class Wallet:
    """A Solana keypair from its secret: base58 (the 64-byte keypair Phantom
    and the BlockRun SDK export, or a 32-byte seed) or solana-keygen's JSON
    array. The secret is never printed; `address` is public."""

    def __init__(self, secret: str):
        text = secret.strip()
        try:
            if text.startswith("["):
                raw = bytes(json.loads(text))
            else:
                raw = b58decode(text)
        except (ValueError, TypeError):
            raise ValueError("the wallet's secret isn't base58 or a JSON byte array") from None
        if len(raw) not in (32, 64):
            raise ValueError(f"the wallet's secret is {len(raw)} bytes, not a 32-byte seed or a 64-byte keypair")
        self._seed = raw[:32]
        self.public = public_key(self._seed)
        if len(raw) == 64 and raw[32:] != self.public:
            raise ValueError("the wallet's secret is a 64-byte keypair whose public half doesn't match its seed")
        self.address = b58encode(self.public)

    def sign(self, message: bytes) -> bytes:
        return sign(self._seed, message)

    def __repr__(self):
        return f"Wallet({self.address})"


# --- x402 ---------------------------------------------------------------------


class PaymentRefused(Exception):
    """A payment the proxy won't make, or the gateway refused for good. The
    reason is safe to show the agent: it names no secret."""

    def __init__(self, reason: str, status: int = 402):
        super().__init__(reason)
        self.reason, self.status = reason, status


def network_id(name: str) -> str:
    return NETWORKS.get(name, name)


def header(response, name: str) -> str | None:
    """A response header, from http.client's response or anything with getheader."""
    return response.getheader(name)


def required(response, body: bytes) -> dict:
    """A 402's quote: the `PAYMENT-REQUIRED` header (base64 JSON), else the body."""
    header_value = header(response, "PAYMENT-REQUIRED") or header(response, "X-PAYMENT-REQUIRED")
    try:
        if header_value:
            quote = json.loads(base64.b64decode(header_value + "=" * (-len(header_value) % 4), validate=True))
        else:
            quote = json.loads(body)
    except (ValueError, binascii.Error):
        raise PaymentRefused("the provider's 402 holds no quote that can be read") from None
    if not isinstance(quote, dict) or not isinstance(quote.get("accepts"), list):
        raise PaymentRefused("the provider's 402 holds no quote that can be read")
    return quote


def choose(quote: dict, network: str) -> dict:
    """The quote's `exact` requirement on `network` in a known asset, checked
    field by field, or PaymentRefused."""
    if quote.get("x402Version") != 2:
        raise PaymentRefused(f"x402 version {str(quote.get('x402Version'))[:8]!r} isn't supported")
    for option in quote["accepts"]:
        if not isinstance(option, dict) or option.get("scheme") != "exact" or option.get("network") != network:
            continue
        if (network, option.get("asset")) not in ASSETS:
            continue
        extra = option.get("extra")
        if not isinstance(extra, dict):
            raise PaymentRefused("the quote has no extra (fee payer, blockhash)")
        try:
            pubkey(option.get("payTo"))
            pubkey(extra.get("feePayer"))
            pubkey(extra.get("recentBlockhash"))
        except ValueError:
            raise PaymentRefused("the quote's payTo, feePayer or recentBlockhash isn't a Solana address") from None
        amount = option.get("amount")
        if not isinstance(amount, str) or not re.fullmatch(r"[0-9]{1,18}", amount) or int(amount) <= 0:
            raise PaymentRefused("the quote's amount isn't a positive integer")
        versions = extra.get("transactionVersions", [0])
        if not isinstance(versions, list) or 0 not in versions:
            raise PaymentRefused("the quote doesn't take a v0 transaction")
        memo = extra.get("memo")
        if memo is not None and (not isinstance(memo, str) or len(memo.encode()) > MAX_MEMO_BYTES):
            raise PaymentRefused("the quote's memo is too long")
        if option["payTo"] == extra["feePayer"]:
            raise PaymentRefused("the quote pays its own fee payer")
        return option
    raise PaymentRefused(f"the quote has no exact payment in USDC on {network}")


def usd(option: dict) -> float:
    return int(option["amount"]) / 10 ** ASSETS[(option["network"], option["asset"])][1]


def transaction(wallet: Wallet, option: dict, memo: bytes | None = None) -> bytes:
    """The partly signed transfer for an `exact` requirement: the wallet's
    signature in place, the fee payer's left for the facilitator."""
    extra = option["extra"]
    token_program_text, decimals = ASSETS[(option["network"], option["asset"])]
    token_program, mint = pubkey(token_program_text), pubkey(option["asset"])
    fee_payer = pubkey(extra["feePayer"])
    if fee_payer == wallet.public or pubkey(option["payTo"]) == wallet.public:
        raise PaymentRefused("the quote names this wallet as its fee payer or payee")
    source = token_account(wallet.public, mint, token_program)
    destination = token_account(pubkey(option["payTo"]), mint, token_program)
    budget = pubkey(COMPUTE_BUDGET_PROGRAM)
    if memo is None:
        seller = extra.get("memo")
        memo = seller.encode() if isinstance(seller, str) and seller else binascii.hexlify(os.urandom(16))
    instructions = [
        (budget, [], bytes([2]) + COMPUTE_UNIT_LIMIT.to_bytes(4, "little")),
        (budget, [], bytes([3]) + COMPUTE_UNIT_PRICE_MICROLAMPORTS.to_bytes(8, "little")),
        (
            token_program,
            [(source, False, True), (mint, False, False), (destination, False, True), (wallet.public, True, False)],
            bytes([12]) + int(option["amount"]).to_bytes(8, "little") + bytes([decimals]),
        ),
        (pubkey(MEMO_PROGRAM), [], memo),
    ]
    message = compile_v0(fee_payer, instructions, pubkey(extra["recentBlockhash"]))
    return shortvec(2) + bytes(64) + wallet.sign(message) + message


def payment_header(wallet: Wallet, quote: dict, option: dict, memo: bytes | None = None) -> str:
    """`PAYMENT-SIGNATURE`'s value: the x402 v2 payload the SDK's client sends
    (payload, the requirement taken, and the quote's resource and extensions)."""
    payload = {
        "x402Version": 2,
        "payload": {"transaction": base64.b64encode(transaction(wallet, option, memo)).decode()},
        "accepted": option,
    }
    for name in ("resource", "extensions"):
        if quote.get(name):
            payload[name] = quote[name]
    return base64.b64encode(json.dumps(payload, separators=(",", ":")).encode()).decode()


def _norm(text: object) -> str:
    return re.sub(r"[^a-z0-9]", "", str(text or "").lower())


PRE_BROADCAST_CODES = {"paymentunderpaid", "paymentreplay", "paymentinvalid"}
PRE_BROADCAST_TITLES = ("paymentverificationfailed", "paymentauthorizationalreadyused", "paymentbelowquotedprice")
SETTLEMENT = "settlementfailed"
# A fresh signature can't cure these: no USDC, no token account, a key or
# payer the facilitator won't take.
OUT_OF_FUNDS = ("insufficientfunds", "insufficientbalance")
UNRECOVERABLE = OUT_OF_FUNDS + ("invalidaccountdata", "accountnotfound", "invalidsignature", "denylisted")


def refusal(body: bytes) -> dict:
    try:
        parsed = json.loads(body)
    except ValueError:
        return {}
    return parsed if isinstance(parsed, dict) else {}


def out_of_funds(body: dict) -> bool:
    text = _norm(json.dumps(body))
    return any(p in text for p in OUT_OF_FUNDS)


def safe_to_resign(body: dict) -> bool:
    """Whether a 402 to a signed call proves no transfer was broadcast, so a
    new quote and signature can't pay twice (the BlockRun SDK's
    `_is_safe_resign_error`)."""
    if any(body.get(k) for k in ("poll_url", "job_id", "recoverable", "wallet_auth")):
        return False
    code, reason = _norm(body.get("code")), _norm(body.get("reason"))
    title = _norm(body.get("error") if isinstance(body.get("error"), str) else body.get("message"))
    if SETTLEMENT in (code, reason) or title.startswith("payment" + SETTLEMENT):
        return False
    text = _norm(json.dumps(body))
    if any(p in text for p in UNRECOVERABLE):
        return False
    return code in PRE_BROADCAST_CODES or title.startswith(PRE_BROADCAST_TITLES)


def reports_payment(response, body: dict) -> bool:
    """Whether a signed call's 5xx says its payment did or may have moved:
    then it is never sent again (the SDK's `_paid_5xx_reports_payment`)."""
    payment = body.get("payment_status")
    if isinstance(payment, str) and payment and payment != "not_charged":
        return True
    if body.get("tx_hash") or body.get("code") == "CHARGED_UPSTREAM_FAILED":
        return True
    return bool(header(response, "X-Payment-Receipt") or header(response, "PAYMENT-RESPONSE"))


class Ledger:
    """What the batch has signed for, in USD, against an optional budget. A
    payment is held before it is signed and let go if the gateway proves it
    never moved; anything else counts, since it may have settled.

    With a `path`, the ledger starts from what that file holds and writes
    every change back, so the batch's paid preflight and its key proxy, one
    after the other, spend against the same budget (MTX_X402_LEDGER)."""

    def __init__(self, budget: float | None = None, path: str | None = None):
        self.budget = budget
        self.path = path
        self.spent = 0.0
        self.calls = 0
        self.lock = threading.Lock()
        if path and os.path.exists(path):
            try:
                with open(path) as f:
                    saved = json.load(f)
                self.spent, self.calls = float(saved["spent"]), int(saved["calls"])
            except (OSError, ValueError, KeyError, TypeError):
                raise ValueError(f"the x402 ledger {path} can't be read") from None

    @classmethod
    def from_env(cls) -> "Ledger":
        """The batch's ledger: MTX_X402_BUDGET_USD and MTX_X402_LEDGER."""
        budget = os.environ.get("MTX_X402_BUDGET_USD", "").strip()
        try:
            limit = float(budget) if budget else None
        except ValueError:
            raise ValueError("MTX_X402_BUDGET_USD must be a number of US dollars") from None
        if limit is not None and not limit >= 0:
            raise ValueError("MTX_X402_BUDGET_USD must be a number of US dollars")
        return cls(limit, os.environ.get("MTX_X402_LEDGER") or None)

    def _save(self) -> None:
        if not self.path:
            return
        temporary = f"{self.path}.{os.getpid()}.tmp"
        with open(temporary, "w") as f:
            json.dump({"spent": round(self.spent, 9), "calls": self.calls}, f)
        os.replace(temporary, self.path)

    def hold(self, amount: float) -> None:
        with self.lock:
            if self.budget is not None and self.spent + amount > self.budget + 1e-12:
                raise PaymentRefused(
                    f"x402 budget spent: ${self.spent:.6f} of ${self.budget:.2f} (MTX_X402_BUDGET_USD); "
                    f"this call is quoted ${amount:.6f}"
                )
            self.spent += amount
            self.calls += 1
            self._save()

    def release(self, amount: float) -> None:
        with self.lock:
            self.spent -= amount
            self.calls -= 1
            self._save()


# A refused payment the gateway proves never moved is signed again, on a new
# quote, at most this many times (the SDK's _MAX_PAYMENT_RETRIES and backoffs).
RESIGNS = 4
RESIGN_BACKOFF = (0.25, 0.5, 1.0, 2.0)
STATUS_5XX = {500, 502, 503, 504}


def pay(send, wallet: Wallet, network: str, cap: float, ledger: Ledger):
    """Make one model call on an x402 route. `send(extra_headers)` sends the
    call and returns (connection, response) with the response's head read.

    The call goes out unsigned first: a free model answers it, and a paid one
    quotes it (402). A quote within `cap` (USD) and the ledger's budget is
    signed, and the call sent again with it. A 5xx to a signed call that
    doesn't report a payment is sent once more with the same signature, which
    settles at most once. A 402 to a signed call is signed again, on a new
    quote, only when it proves nothing was broadcast. Returns (connection,
    response, USD signed for) for the caller to relay, or raises
    PaymentRefused."""
    connection, response = send({})
    if response.status != 402:
        return connection, response, 0.0
    for attempt in range(RESIGNS + 1):
        data = response.read()
        connection.close()
        quote = required(response, data)
        option = choose(quote, network)
        price = usd(option)
        if price > cap:
            raise PaymentRefused(
                f"the provider quotes ${price:.6f} for this call, over the route's cap of ${cap:.2f} a call", 403
            )
        ledger.hold(price)
        try:
            header = payment_header(wallet, quote, option)
        except Exception:
            ledger.release(price)
            raise
        connection, response = send({"PAYMENT-SIGNATURE": header})
        resent = False
        if response.status in STATUS_5XX:
            data = response.read()
            if reports_payment(response, refusal(data)):
                return connection, _Replay(response, data, paid=True), price
            connection.close()
            connection, response = send({"PAYMENT-SIGNATURE": header})
            resent = True
            if response.status >= 500:
                # Twice failed on one signature: it may have settled.
                return connection, _Replay(response, response.read(), paid=True), price
        if response.status != 402:
            return connection, response, price
        data = response.read()
        connection.close()
        refused = refusal(data)
        # After a resend, the 402 may be the gateway refusing a signature it
        # already settled on the first send: never signed again.
        if not resent and safe_to_resign(refused):
            ledger.release(price)
            if attempt < RESIGNS:
                time.sleep(RESIGN_BACKOFF[min(attempt, len(RESIGN_BACKOFF) - 1)])
                connection, response = send({})
                if response.status != 402:
                    return connection, response, 0.0
                continue
        detail = str(refused.get("message") or refused.get("error") or refused.get("reason") or "")[:200]
        if out_of_funds(refused):
            raise PaymentRefused(f"x402 wallet is out of USDC: fund {wallet.address} on Solana ({detail})")
        raise PaymentRefused(f"the provider refused the x402 payment: {detail}")
    raise PaymentRefused("the provider refused the x402 payment")


class _Replay:
    """A response whose body was already read, relayed as is, unless it failed
    after a payment that did or may have settled (`paid`). Then it goes to the
    agent as 424 with `x-should-retry: false`: an OpenAI or Anthropic client
    retries a 5xx on its own (they retry 408, 409, 429 and 5xx, and honour the
    header), and each retry would be a new call, paid again."""

    def __init__(self, response, data: bytes, paid: bool = False):
        self.status, self.reason, self._data = response.status, response.reason, data
        self._headers = response.getheaders()
        if paid:
            self.status, self.reason = 424, "Failed Dependency"
            self._headers = [(k, v) for k, v in self._headers if k.lower() != "x-should-retry"]
            self._headers.append(("x-should-retry", "false"))

    def getheader(self, name, default=None):
        for k, v in self._headers:
            if k.lower() == name.lower():
                return v
        return default

    def getheaders(self):
        return self._headers

    def read(self, *_):
        data, self._data = self._data, b""
        return data

    read1 = read

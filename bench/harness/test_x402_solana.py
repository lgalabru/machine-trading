"""The x402 Solana signer, and the key proxy paying for calls with it.

    python3 -I bench/harness/test_x402_solana.py

The signer against RFC 8032's vectors and against bytes the x402 SDK
(x402[svm] 2.26, solders 0.27) produced for the same wallet, quote and memo;
then the key proxy, on its own process, in front of a stand-in x402 gateway
on this machine: it pays a quote, re-signs only what the gateway proves never
moved, resends a 5xx with the same signature, refuses a quote over the cap or
the batch's budget, and never lets the wallet's secret out.
"""

import base64
import hashlib
import http.client
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).parent
sys.path.insert(0, str(HERE))
import models  # noqa: E402
import x402_solana as x  # noqa: E402

USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
PAY_TO = "AQqnMFBwGZEoti85aTVRy8XYpKrho7GaMDx9ZB3CEeKA"
FEE_PAYER = "2wKupLR9q6wXYppw8Gr2NvWxKBUqm4PPJKkQfoxHDBg4"
BLOCKHASH = "EhKkojULUuaNZ1svjSo7WauQ5kGuF5PZVH233k3mdYq2"

# A wallet made for this test (sha256 of "golden" as its seed), as solders
# writes it, and what solders and the x402 SDK derive from it.
SEED = hashlib.sha256(b"golden").digest()
KEYPAIR = "5RfeeTLA8mFbyp8bGicScVE5Z9gNYfyUbeNDUsCi3vuatN243jztFJZ6z35irJd3EdbfsfF9c6JZezeDjibkhdEX"
ADDRESS = "7vMfLJyXkKBknGyUJp2BAhuyWNuvs5z5RZDzoZC9HYnb"
ATA = "4fLJQfsAxFCh4rVCcCGht1oX49DNJCtSP9BgZaiCbBUL"
PAY_TO_ATA = "2V47kNnc5hpvPDuZjVKvktfZnPdk5Dac96BZkLJDYNsR"
# ExactSvmScheme.create_payment_payload for OPTION, the memo's 16 random
# bytes fixed to bytes.fromhex(MEMO): the transaction, base64.
MEMO = b"0123456789abcdef0123456789abcdef"
SDK_TRANSACTION = (
    "AgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2LpAMWUqqlf9hKxz4pmEq"
    "FGXERS2Z8vTMz/5hcFFsRfRiG1mC8T8nkZwdepruvqL7J42cY2/PcyQxormqdFMEgAIBBAgcxm7UN1ERWkK1kFEyxPZoMoYR5ejet9hiWF0q"
    "NVHQAWbRRAv0fCP4Opd2LaNapYkdgHNJR9+6wve8FpSkYRaQFgt5JkEtut+08T5cLrEFszJ7UAqMeCZaiJMHhZfquNg2ZPRKew6l5B33XyNC"
    "eQFZf2XSZQlIkXwiHH15v+6SeQMGRm/lIRcy/+ytunLDm+e8jOW7xfcSayxDmzpAAAAABUpTWpkpIQZNJOhxYNo4fHw1td28kruB5B+oQEEF"
    "RI0G3fbh12Whk9nL4UbO63msHLSF7V9bN5E6jPWFfv8Aqcb6evO+2606PWXzaqvJdDGxu+TC0vbg5HymAgNFL11hy3xeepsgVCvMQ2ACigdc"
    "LwbIUrkSHZQhGSCsqROW150EBAAFAiBOAAAEAAkDAQAAAAAAAAAGBAMHAgEKDJNQAQAAAAAABgUAIDAxMjM0NTY3ODlhYmNkZWYwMTIzNDU2"
    "Nzg5YWJjZGVmAA=="
)


def option(amount="86163", network=x.SOLANA_MAINNET, **extra):
    return {
        "scheme": "exact", "network": network, "amount": amount, "payTo": PAY_TO, "maxTimeoutSeconds": 300,
        "asset": USDC,
        "extra": {"feePayer": FEE_PAYER, "recentBlockhash": BLOCKHASH, "lastValidBlockHeight": "450418544", **extra},
    }


def quote(*options):
    return {
        "x402Version": 2, "error": "Payment required", "accepts": list(options),
        "resource": {"url": "https://sol.blockrun.ai/api/v1/chat/completions", "mimeType": "application/json"},
    }


class Signer(unittest.TestCase):
    def test_rfc8032_vectors(self):
        for secret, public, message, signature in (
            ("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
             "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a", "",
             "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"),
            ("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
             "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c", "72",
             "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"),
            ("c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
             "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025", "af82",
             "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a"),
        ):
            seed, msg = bytes.fromhex(secret), bytes.fromhex(message)
            self.assertEqual(x.public_key(seed).hex(), public)
            self.assertEqual(x.sign(seed, msg).hex(), signature)
            self.assertTrue(x.verify(bytes.fromhex(public), msg, bytes.fromhex(signature)))
            self.assertFalse(x.verify(bytes.fromhex(public), msg + b"!", bytes.fromhex(signature)))

    def test_wallet_reads_every_export(self):
        for secret in (KEYPAIR, x.b58encode(SEED), json.dumps(list(x.b58decode(KEYPAIR)))):
            self.assertEqual(x.Wallet(secret).address, ADDRESS)
        self.assertNotIn(KEYPAIR, repr(x.Wallet(KEYPAIR)))
        mismatched = x.b58encode(SEED + bytes(32))
        for bad in ("", "0OIl", mismatched, x.b58encode(b"short"), "[1, 2"):
            with self.assertRaises(ValueError):
                x.Wallet(bad)

    def test_token_accounts_match_solders(self):
        mint, program = x.pubkey(USDC), x.pubkey(x.TOKEN_PROGRAM)
        self.assertEqual(x.b58encode(x.token_account(x.pubkey(ADDRESS), mint, program)), ATA)
        self.assertEqual(x.b58encode(x.token_account(x.pubkey(PAY_TO), mint, program)), PAY_TO_ATA)

    def test_transaction_matches_the_sdk_byte_for_byte(self):
        tx = x.transaction(x.Wallet(KEYPAIR), option(), MEMO)
        self.assertEqual(base64.b64encode(tx).decode(), SDK_TRANSACTION)
        # Two signatures, the fee payer's left empty; the wallet's verifies.
        self.assertEqual(tx[0], 2)
        self.assertEqual(tx[1:65], bytes(64))
        self.assertTrue(x.verify(x.pubkey(ADDRESS), tx[129:], tx[65:129]))

    def test_header_carries_the_sdks_payload(self):
        q = quote(option())
        header = json.loads(base64.b64decode(x.payment_header(x.Wallet(KEYPAIR), q, option(), MEMO)))
        self.assertEqual(header["x402Version"], 2)
        self.assertEqual(header["accepted"], option())
        self.assertEqual(header["resource"], q["resource"])
        self.assertEqual(header["payload"]["transaction"], SDK_TRANSACTION)

    def test_memo_is_random_unless_the_seller_sets_one(self):
        wallet = x.Wallet(KEYPAIR)
        self.assertNotEqual(x.transaction(wallet, option()), x.transaction(wallet, option()))
        seller = option(memo="order-42")
        self.assertIn(b"order-42", x.transaction(wallet, seller))
        self.assertEqual(x.transaction(wallet, seller), x.transaction(wallet, seller))

    def test_choose_takes_only_a_well_formed_exact_usdc_quote(self):
        upto = dict(option(), scheme="upto")
        batch = dict(option(), scheme="batch-settlement")
        self.assertEqual(x.choose(quote(batch, upto, option()), x.SOLANA_MAINNET), option())
        refused = {
            "only upto": quote(upto),
            "other network": quote(option(network=x.SOLANA_DEVNET)),
            "other asset": quote(dict(option(), asset=PAY_TO)),
            "no blockhash": quote(option(recentBlockhash=None)),
            "bad fee payer": quote(option(feePayer="nope")),
            "zero": quote(option(amount="0")),
            "not digits": quote(option(amount="1e6")),
            "no v0": quote(option(transactionVersions=["legacy"])),
            "long memo": quote(option(memo="m" * 257)),
            "x402 v1": dict(quote(option()), x402Version=1),
            "pays the fee payer": quote(dict(option(), payTo=FEE_PAYER)),
        }
        for name, q in refused.items():
            with self.subTest(name), self.assertRaises(x.PaymentRefused):
                x.choose(q, x.SOLANA_MAINNET)
        with self.assertRaises(x.PaymentRefused):
            x.transaction(x.Wallet(KEYPAIR), dict(option(), payTo=ADDRESS))

    def test_resign_only_what_never_moved(self):
        safe = [
            {"error": "Payment verification failed", "code": "PAYMENT_REPLAY"},
            {"error": "x", "code": "PAYMENT_UNDERPAID"},
            {"error": "Payment verification failed", "reason": "expired_signature"},
            {"error": "Payment below quoted price"},
        ]
        terminal = [
            {"error": "Payment settlement failed", "code": "SETTLEMENT_FAILED"},
            {"error": "Payment verification failed", "reason": "settlement_failed"},
            {"error": "Payment verification failed", "code": "PAYMENT_INVALID", "reason": "insufficient_funds"},
            {"error": "Payment verification failed", "code": "PAYMENT_REPLAY", "poll_url": "/v1/x"},
            {"error": "Payment required"},
            {},
        ]
        for body in safe:
            self.assertTrue(x.safe_to_resign(body), body)
        for body in terminal:
            self.assertFalse(x.safe_to_resign(body), body)
        self.assertTrue(x.out_of_funds({"reason": "insufficient_funds"}))

    def test_ledger_persists_between_processes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "ledger.json")
            x.Ledger(1.0, path).hold(0.25)
            again = x.Ledger(1.0, path)
            self.assertEqual((again.spent, again.calls), (0.25, 1))
            with open(path, "w") as f:
                f.write("{")
            with self.assertRaises(ValueError):
                x.Ledger(1.0, path)

    def test_ledger_holds_and_releases(self):
        ledger = x.Ledger(budget=0.0025)
        ledger.hold(0.001)
        ledger.hold(0.001)
        with self.assertRaisesRegex(x.PaymentRefused, "x402 budget spent"):
            ledger.hold(0.001)
        ledger.release(0.001)
        ledger.hold(0.001)
        self.assertEqual(ledger.calls, 2)


class Gateway(BaseHTTPRequestHandler):
    """A stand-in for BlockRun's Solana gateway: a 402 quote for an unsigned
    call, the call answered for a signature that verifies, each model a
    different gateway behaviour."""

    protocol_version = "HTTP/1.0"
    seen: list = []
    quotes = {"stub/dear": "10000000", "stub/flaky": "2000"}
    lock = threading.Lock()

    def log_message(self, *args):
        pass

    def answer(self, status, body, headers=()):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        body = json.loads(raw)
        model = body["model"]
        signature = self.headers.get("PAYMENT-SIGNATURE")
        with self.lock:
            Gateway.seen.append({"model": model, "signature": signature, "headers": dict(self.headers), "body": body})
            signed = sum(1 for s in Gateway.seen if s["model"] == model and s["signature"])
        if model == "stub/keyed":
            ok = self.headers.get("Authorization") == "Bearer sk-stub-key-0123456789" and not signature
            return self.answer(200 if ok else 401, {"choices": [{"message": {"content": "keyed"}}]})
        if model == "stub/free":
            return self.answer(200, {"choices": [{"message": {"content": "free"}}]})
        if not signature:
            network = x.SOLANA_DEVNET if model == "stub/devnet" else x.SOLANA_MAINNET
            q = quote(option(amount=self.quotes.get(model, "1000"), network=network))
            header = base64.b64encode(json.dumps(q).encode()).decode()
            return self.answer(402, {"x402Version": 2, "error": "Payment required"}, [("PAYMENT-REQUIRED", header)])
        payment = json.loads(base64.b64decode(signature))
        tx = base64.b64decode(payment["payload"]["transaction"])
        if not x.verify(x.pubkey(ADDRESS), tx[129:], tx[65:129]):
            return self.answer(402, {"error": "Payment verification failed", "reason": "invalid_signature"})
        if model == "stub/replay" and signed == 1:
            return self.answer(402, {"error": "Payment verification failed", "code": "PAYMENT_REPLAY"})
        if model == "stub/settle":
            return self.answer(402, {"error": "Payment settlement failed", "code": "SETTLEMENT_FAILED"})
        if model == "stub/broke":
            return self.answer(402, {"error": "Payment verification failed", "code": "PAYMENT_INVALID", "reason": "insufficient_funds"})
        if model == "stub/flaky" and signed == 1:
            return self.answer(502, {"error": "bad gateway"})
        if model == "stub/charged":
            return self.answer(500, {"error": "upstream failed", "payment_status": "settled", "tx_hash": "abc"})
        if model == "stub/down":
            return self.answer(502, {"error": "bad gateway"})
        stream = b'data: {"choices":[{"delta":{"content":"ok"}}]}\n\ndata: [DONE]\n\n'
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("PAYMENT-RESPONSE", "receipt-naming-the-wallet")
        self.end_headers()
        self.wfile.write(stream)


MODELS = ["ok", "free", "replay", "settle", "broke", "dear", "flaky", "charged", "down", "devnet", "room"]


class Proxy(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
        threading.Thread(target=cls.gateway.serve_forever, daemon=True).start()
        upstream = f"http://127.0.0.1:{cls.gateway.server_address[1]}/api/v1"
        cls.tmp = tempfile.TemporaryDirectory()
        registry = Path(cls.tmp.name) / "models.toml"
        tables = []
        for name in MODELS:
            room = "max_tokens = 1234\n" if name == "room" else ""
            tables.append(
                f'[pay-{name}]\napi = "chat"\npay = "x402-solana"\nupstream = "{upstream}"\n'
                f'model = "stub/{name}"\nkey = "SOLANA_WALLET_KEY"\nmax_usd = 1.5\n{room}'
            )
        tables.append(f'[pay-keyed]\napi = "chat"\nupstream = "{upstream}"\nmodel = "stub/keyed"\nkey = "STUB_API_KEY"\n')
        registry.write_text("\n".join(tables))
        cls.secret = "a" * 64
        cls.env = {
            "PATH": os.environ.get("PATH", ""), "MTX_MODELS": str(registry), "MTX_PROXY_SECRET": cls.secret,
            "SOLANA_WALLET_KEY": KEYPAIR, "STUB_API_KEY": "sk-stub-key-0123456789",
        }
        # The tokens the proxy checks: HMACs under the same secret.
        os.environ["MTX_PROXY_SECRET"] = cls.secret

    @classmethod
    def tearDownClass(cls):
        cls.gateway.shutdown()
        cls.tmp.cleanup()

    def setUp(self):
        with Gateway.lock:
            Gateway.seen.clear()

    def start(self, budget=None):
        env = dict(self.env)
        if budget is not None:
            env["MTX_X402_BUDGET_USD"] = str(budget)
        proxy = subprocess.Popen(
            [sys.executable, "-I", str(HERE / "key_proxy.py"), "--host", "127.0.0.1", "--port", "0",
             *(f"pay-{m}" for m in MODELS), "pay-keyed"],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        url = proxy.stdout.readline().strip()
        self.assertTrue(url.startswith("http://127.0.0.1:"), url)
        self.addCleanup(self.stop, proxy)
        return proxy, url

    def stop(self, proxy):
        if proxy.returncode is not None:
            return
        proxy.terminate()
        _, err = proxy.communicate(timeout=10)
        self.assertNotIn(KEYPAIR, err)
        self.logs = err

    def call(self, url, name, **extra):
        host = url.removeprefix("http://")
        connection = http.client.HTTPConnection(host, timeout=30)
        body = {"model": f"stub/{name}", "messages": [{"role": "user", "content": "hi"}], "stream": True, **extra}
        token = models.route_token(f"pay-{name}")
        connection.request(
            "POST", f"/pay-{name}/chat/completions", body=json.dumps(body),
            headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
        )
        response = connection.getresponse()
        data = response.read()
        headers = dict(response.getheaders())
        connection.close()
        self.assertNotIn(KEYPAIR.encode(), data)
        self.assertNotIn(KEYPAIR, json.dumps(headers))
        return response.status, headers, data

    def calls_to(self, name):
        return [s for s in Gateway.seen if s["model"] == f"stub/{name}"]

    def test_pays_a_quote_and_relays_the_stream(self):
        _, url = self.start()
        status, headers, data = self.call(url, "ok")
        self.assertEqual(status, 200)
        self.assertIn(b"[DONE]", data)
        self.assertNotIn("PAYMENT-RESPONSE", {k.upper() for k in headers})
        seen = self.calls_to("ok")
        self.assertEqual([bool(s["signature"]) for s in seen], [False, True])
        for s in seen:
            self.assertNotIn("authorization", {k.lower() for k in s["headers"]})
            self.assertNotIn(KEYPAIR, json.dumps(s))

    def test_a_keyed_route_is_unchanged(self):
        _, url = self.start()
        status, _, data = self.call(url, "keyed")
        self.assertEqual(status, 200)
        self.assertNotIn(b"sk-stub-key", data)
        self.assertEqual([bool(s["signature"]) for s in self.calls_to("keyed")], [False])

    def test_free_model_is_never_paid(self):
        _, url = self.start()
        self.assertEqual(self.call(url, "free")[0], 200)
        self.assertEqual([bool(s["signature"]) for s in self.calls_to("free")], [False])

    def test_resigns_a_replay_on_a_new_quote(self):
        proxy, url = self.start()
        self.assertEqual(self.call(url, "replay")[0], 200)
        seen = self.calls_to("replay")
        self.assertEqual([bool(s["signature"]) for s in seen], [False, True, False, True])
        self.assertNotEqual(seen[1]["signature"], seen[3]["signature"])
        self.stop(proxy)
        self.assertIn("batch_calls=1", self.logs)

    def test_never_resigns_after_settlement(self):
        _, url = self.start()
        status, _, data = self.call(url, "settle")
        self.assertEqual(status, 402)
        self.assertIn(b"refused the x402 payment", data)
        self.assertEqual(sum(bool(s["signature"]) for s in self.calls_to("settle")), 1)

    def test_out_of_funds_stops_the_batch(self):
        _, url = self.start()
        status, _, data = self.call(url, "broke")
        self.assertEqual(status, 402)
        self.assertIn(b"x402 wallet is out of USDC", data)
        self.assertIn(ADDRESS.encode(), data)
        self.assertEqual(sum(bool(s["signature"]) for s in self.calls_to("broke")), 1)

    def test_refuses_a_quote_over_the_cap_unsigned(self):
        _, url = self.start()
        status, _, data = self.call(url, "dear")
        self.assertEqual(status, 403)
        self.assertIn(b"over the route's cap", data)
        self.assertFalse(any(s["signature"] for s in self.calls_to("dear")))

    def test_refuses_a_quote_on_another_network(self):
        _, url = self.start()
        self.assertEqual(self.call(url, "devnet")[0], 402)
        self.assertFalse(any(s["signature"] for s in self.calls_to("devnet")))

    def test_resends_a_5xx_with_the_same_signature(self):
        _, url = self.start()
        self.assertEqual(self.call(url, "flaky")[0], 200)
        signatures = [s["signature"] for s in self.calls_to("flaky") if s["signature"]]
        self.assertEqual(len(signatures), 2)
        self.assertEqual(signatures[0], signatures[1])

    def test_never_resends_a_5xx_that_reports_a_payment(self):
        _, url = self.start()
        status, headers, data = self.call(url, "charged")
        # Not a 5xx: the agent's client would retry it, and pay again.
        self.assertEqual(status, 424)
        self.assertEqual({k.lower(): v for k, v in headers.items()}.get("x-should-retry"), "false")
        self.assertIn(b"upstream failed", data)
        self.assertEqual(sum(bool(s["signature"]) for s in self.calls_to("charged")), 1)

    def test_a_5xx_still_failing_after_the_resend_is_not_retried(self):
        _, url = self.start()
        status, headers, _ = self.call(url, "down")
        self.assertEqual(status, 424)
        self.assertEqual({k.lower(): v for k, v in headers.items()}.get("x-should-retry"), "false")
        signatures = [s["signature"] for s in self.calls_to("down") if s["signature"]]
        self.assertEqual(len(signatures), 2)
        self.assertEqual(signatures[0], signatures[1])

    def test_preflight_and_proxy_share_the_budget(self):
        with tempfile.TemporaryDirectory() as tmp:
            ledger = os.path.join(tmp, "x402-ledger.json")
            # The preflight spends $0.001 of $0.0015...
            first = x.Ledger(0.0015, ledger)
            first.hold(0.001)
            # ...so the proxy, started after it, has room for no second $0.001 call.
            self.env["MTX_X402_LEDGER"] = ledger
            try:
                _, url = self.start(budget=0.0015)
                status, _, data = self.call(url, "ok")
            finally:
                del self.env["MTX_X402_LEDGER"]
            self.assertEqual(status, 402)
            self.assertIn(b"x402 budget spent", data)
            self.assertFalse(any(s["signature"] for s in self.calls_to("ok")))

    def test_a_zero_budget_refuses_the_preflight(self):
        registry, models.REGISTRY = models.REGISTRY, Path(self.tmp.name) / "models.toml"
        self.addCleanup(setattr, models, "REGISTRY", registry)
        found = models.entry("pay-ok")
        upstream_before = len(Gateway.seen)
        with tempfile.TemporaryDirectory() as tmp:
            ledger = x.Ledger(0.0, os.path.join(tmp, "x402-ledger.json"))
            self.assertFalse(models.paid_preflight("pay-ok", found, KEYPAIR, ledger))
        self.assertFalse(any(s["signature"] for s in Gateway.seen[upstream_before:]))

    def test_batch_budget(self):
        _, url = self.start(budget=0.0015)
        self.assertEqual(self.call(url, "ok")[0], 200)
        status, _, data = self.call(url, "ok")
        self.assertEqual(status, 402)
        self.assertIn(b"x402 budget spent", data)

    def test_route_max_tokens_fills_only_a_gap(self):
        _, url = self.start()
        self.call(url, "room")
        self.call(url, "room", max_tokens=50)
        sent = [s["body"].get("max_tokens") for s in self.calls_to("room") if s["signature"]]
        self.assertEqual(sent, [1234, 50])

    def test_the_token_is_still_required(self):
        _, url = self.start()
        connection = http.client.HTTPConnection(url.removeprefix("http://"), timeout=10)
        connection.request("POST", "/pay-ok/chat/completions", body="{}", headers={"Authorization": "Bearer nope"})
        self.assertEqual(connection.getresponse().status, 401)


class Registry(unittest.TestCase):
    def test_x402_fields_are_checked(self):
        bad = {
            "pay on another API": 'api = "anthropic"\npay = "x402-solana"',
            "unknown pay": 'api = "chat"\npay = "x402-ethereum"',
            "zero cap": 'api = "chat"\npay = "x402-solana"\nmax_usd = 0',
            "unknown network": 'api = "chat"\npay = "x402-solana"\nnetwork = "solana-testnet"',
            "max_tokens not an integer": 'api = "chat"\nmax_tokens = "lots"',
        }
        with tempfile.TemporaryDirectory() as tmp:
            for name, fields in bad.items():
                registry = Path(tmp) / "models.toml"
                registry.write_text(f'[m]\n{fields}\nupstream = "https://x"\nmodel = "a/b"\nkey = "K"\n')
                models.REGISTRY = registry
                with self.subTest(name), self.assertRaises(SystemExit):
                    models.entry("m")
        models.REGISTRY = HERE / "models.toml"
        for name in ("blockrun-claude-sonnet-5-5", "blockrun-glm-5-3"):
            found = models.entry(name)
            self.assertEqual((found["pay"], found["key"]), ("x402-solana", "SOLANA_WALLET_KEY"))
        self.assertIn("SOLANA_WALLET_KEY", models.key_names())


if __name__ == "__main__":
    unittest.main()

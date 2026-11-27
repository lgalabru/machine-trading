"""The key proxy's usage log: reading usage out of answers, adding a trial's
calls up, and the proxy logging a real streamed call under its trial.

    python3 -I bench/harness/test_usage_log.py
"""

import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import usage_log  # noqa: E402

NOUS_JSON = {
    "id": "x", "choices": [{"message": {"content": "ok"}}],
    "usage": {"prompt_tokens": 1200, "completion_tokens": 30,
              "prompt_tokens_details": {"cached_tokens": 1000, "cache_write_tokens": 150},
              "cost": 0.0042},
}


def sse(*events):
    return b"".join(b"data: " + json.dumps(e).encode() + b"\n\n" for e in events) + b"data: [DONE]\n\n"


class UsageOf(unittest.TestCase):
    def test_a_json_answer_with_its_price(self):
        self.assertEqual(usage_log.usage_of(json.dumps(NOUS_JSON).encode()), {
            "input_tokens": 1200, "cached_tokens": 1000, "cache_write_tokens": 150,
            "output_tokens": 30, "cost_usd": 0.0042})

    def test_a_chat_stream_reports_its_usage_in_the_last_event(self):
        data = sse({"choices": [{"delta": {"content": "o"}}]},
                   {"choices": [{"delta": {"content": "k"}}]},
                   {"choices": [], "usage": NOUS_JSON["usage"]})
        self.assertEqual(usage_log.usage_of(data)["cost_usd"], 0.0042)

    def test_an_anthropic_stream_splits_input_and_output(self):
        data = (b"event: message_start\ndata: " + json.dumps({"type": "message_start", "message": {"usage": {
            "input_tokens": 50, "cache_read_input_tokens": 900, "cache_creation_input_tokens": 40,
            "output_tokens": 1}}}).encode() + b"\n\n"
            b"event: message_delta\ndata: " + json.dumps({"type": "message_delta", "usage": {
                "output_tokens": 77}}).encode() + b"\n\n")
        self.assertEqual(usage_log.usage_of(data), {
            "input_tokens": 50, "cached_tokens": 900, "cache_write_tokens": 40, "output_tokens": 77})

    def test_a_responses_stream_reports_in_its_completed_event(self):
        data = sse({"type": "response.output_text.delta", "delta": "ok"},
                   {"type": "response.completed", "response": {"usage": {
                       "input_tokens": 10, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 4}}}})
        self.assertEqual(usage_log.usage_of(data), {"input_tokens": 10, "cached_tokens": 4, "output_tokens": 2})

    def test_an_answer_without_usage_or_unreadable_is_empty(self):
        for data in [b"", b"not json", b"data: {broken\n\n", json.dumps({"choices": []}).encode(),
                     json.dumps({"usage": "weird"}).encode(), json.dumps([1, 2]).encode()]:
            self.assertEqual(usage_log.usage_of(data), {}, data)


class CaptureBounds(unittest.TestCase):
    def test_a_long_answer_keeps_its_tail(self):
        capture = usage_log.Capture()
        filler = b"x" * 65536
        for _ in range(usage_log.KEEP_BYTES // len(filler) + 10):
            capture.add(filler)
        capture.add(b"\ndata: " + json.dumps({"usage": NOUS_JSON["usage"]}).encode() + b"\n\n")
        self.assertLessEqual(len(capture.data()), usage_log.TAIL_BYTES)
        self.assertEqual(usage_log.usage_of(capture.data())["cost_usd"], 0.0042)


class Totals(unittest.TestCase):
    def test_a_trial_adds_up_and_a_missing_price_voids_its_cost(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "usage.jsonl"
            log = usage_log.UsageLog(str(path))
            body = json.dumps(NOUS_JSON).encode()
            log.write("r", "flash-crash-spcx__aaa1111", body)
            log.write("r", "flash-crash-spcx__aaa1111", body)
            log.write("r", "weekend-gap-spcx__bbb2222", body)
            total = usage_log.totals(path, "flash-crash-spcx__aaa1111")
            self.assertEqual(total["calls"], 2)
            self.assertEqual(total["input_tokens"], 2400)
            self.assertAlmostEqual(total["cost_usd"], 0.0084)
            log.write("r", "flash-crash-spcx__aaa1111", b"no usage here")
            self.assertIsNone(usage_log.totals(path, "flash-crash-spcx__aaa1111")["cost_usd"])
            self.assertIsNone(usage_log.totals(path, "nobody__ccc3333"))
            # The proxy sees the trial lowercased; Harbor names it mixed-case.
            log.write("r", "liquidation-flash__j62q5hi", body)
            self.assertEqual(usage_log.totals(path, "liquidation-flash__J62Q5hI")["calls"], 1)
            self.assertIsNone(usage_log.totals(Path(tmp) / "missing.jsonl", "nobody__ccc3333"))

    def test_no_path_logs_nothing(self):
        usage_log.UsageLog(None).write("r", None, b"{}")


class Upstream(BaseHTTPRequestHandler):
    """A provider that streams one answer with its usage."""
    protocol_version = "HTTP/1.0"

    def log_message(self, *args):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
        body = sse({"choices": [{"delta": {"content": "ok"}}]}, {"choices": [], "usage": NOUS_JSON["usage"]})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        self.wfile.write(body)


class ProxyLogsUnderTheTrial(unittest.TestCase):
    def test_a_streamed_call_is_logged_with_its_trial_and_price(self):
        upstream = HTTPServer(("127.0.0.1", 0), Upstream)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        with tempfile.TemporaryDirectory() as tmp:
            registry = Path(tmp) / "models.toml"
            registry.write_text(
                f'[fake]\napi = "chat"\nupstream = "http://127.0.0.1:{upstream.server_address[1]}/v1"\n'
                'model = "x/fake"\nkey = "FAKE_PROVIDER_KEY"\n')
            usage = Path(tmp) / "usage.jsonl"
            env = {**os.environ, "MTX_MODELS": str(registry), "MTX_PROXY_SECRET": "s" * 40,
                   "FAKE_PROVIDER_KEY": "sk-fake-provider-key", "MTX_PROXY_USAGE": str(usage)}
            proxy = subprocess.Popen([sys.executable, "-I", str(HERE / "key_proxy.py"), "--host", "127.0.0.1",
                                      "--port", "0", "fake"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     text=True)
            try:
                url = proxy.stdout.readline().strip()
                self.assertTrue(url.startswith("http://127.0.0.1:"), url)
                token = subprocess.run([sys.executable, "-I", str(HERE / "models.py"), "token", "fake"], env=env,
                                       capture_output=True, text=True, check=True).stdout.strip()
                body = json.dumps({"model": "x/fake", "messages": [{"role": "user", "content": "hi"}],
                                   "stream": True}).encode()

                def call(path):
                    request = urllib.request.Request(url + path, data=body, method="POST", headers={
                        "Authorization": f"Bearer {token}", "Content-Type": "application/json"})
                    return urllib.request.urlopen(request, timeout=10).read()

                answer = call("/fake/t/flash-crash-spcx__cq8nbXG/chat/completions")
                self.assertIn(b'"cost": 0.0042', answer)
                call("/fake/chat/completions")
                with self.assertRaises(urllib.error.HTTPError) as refused:
                    call("/fake/t/not..a..trial/chat/completions")
                self.assertEqual(refused.exception.code, 404)
                with self.assertRaises(urllib.error.HTTPError) as refused:
                    call("/fake/t/Bad_Name/chat/completions")
                self.assertEqual(refused.exception.code, 404)
                for _ in range(50):
                    if usage.exists() and len(usage.read_text().splitlines()) >= 2:
                        break
                    time.sleep(0.1)
                lines = [json.loads(line) for line in usage.read_text().splitlines()]
                self.assertEqual(len(lines), 2)
                self.assertEqual(lines[0]["trial"], "flash-crash-spcx__cq8nbXG")
                self.assertEqual(lines[0]["route"], "fake")
                self.assertEqual(lines[0]["cost_usd"], 0.0042)
                self.assertIsNone(lines[1]["trial"])
                self.assertNotIn("sk-fake-provider-key", usage.read_text())
            finally:
                proxy.terminate()
                proxy.wait(5)
                proxy.stdout.close()
                proxy.stderr.close()
                upstream.shutdown()
                upstream.server_close()


class AdapterNamesItsTrialAndReadsItsCost(unittest.TestCase):
    """The Hermes adapter's side; needs Harbor, so it runs where the bench does."""

    def setUp(self):
        try:
            import mtx_hermes  # noqa: F401
        except ImportError as e:
            self.skipTest(f"harbor isn't importable here: {e}")

    def test_the_route_names_the_trial_once(self):
        import mtx_hermes
        config = {"custom_providers": [{"base_url": "http://10.200.0.1:4000/claude-opus-5-5", "model": "m"}]}
        mtx_hermes.name_the_trial(config, "flash-crash-spcx__cq8nbXG")
        mtx_hermes.name_the_trial(config, "flash-crash-spcx__cq8nbXG")
        self.assertEqual(config["custom_providers"][0]["base_url"],
                         "http://10.200.0.1:4000/claude-opus-5-5/t/flash-crash-spcx__cq8nbXG")
        untouched = {"custom_providers": [{"base_url": "http://x/r"}]}
        mtx_hermes.name_the_trial(untouched, None)
        self.assertEqual(untouched["custom_providers"][0]["base_url"], "http://x/r")

class FinishTrials(unittest.TestCase):
    """finish_trials.py, after Harbor: no Harbor needed while a trial already
    has its trajectory."""

    def test_finishing_a_trial_writes_its_cost_and_keeps_the_original(self):
        import finish_trials
        with tempfile.TemporaryDirectory() as tmp:
            trial = Path(tmp) / "job" / "flash-crash-spcx__cq8nbXG"
            (trial / "agent").mkdir(parents=True)
            (trial / "agent" / "trajectory.json").write_text("{}")
            (trial / "result.json").write_text(json.dumps({"trial_name": trial.name, "agent_result": {"cost_usd": None}}))
            usage = Path(tmp) / "usage.jsonl"
            log = usage_log.UsageLog(str(usage))
            for _ in range(3):
                log.write("r", trial.name.lower(), json.dumps(NOUS_JSON).encode())
            done = finish_trials.finish_trial(trial, usage)
            self.assertEqual(done, {"trajectory": "kept", "cost": "$0.0126"})
            result = json.loads((trial / "result.json").read_text())
            self.assertAlmostEqual(result["agent_result"]["cost_usd"], 0.0126)
            self.assertEqual(result["agent_result"]["n_input_tokens"], 3600)
            self.assertIsNone(json.loads((trial / "result.json.orig").read_text())["agent_result"]["cost_usd"])

    def test_a_tool_result_cut_off_mid_call_is_dropped_before_converting(self):
        import finish_trials
        session = {"source": "oneshot", "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "function": {"name": "terminal"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"},
            {"role": "tool", "tool_call_id": "", "content": "cut off"}]}
        kept = json.loads(finish_trials.without_orphan_results(json.dumps(session)))["messages"]
        self.assertEqual([m.get("tool_call_id") for m in kept if m["role"] == "tool"], ["c1"])


if __name__ == "__main__":
    unittest.main()

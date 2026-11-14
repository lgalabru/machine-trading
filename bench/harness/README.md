# Running a model in Hermes

Every model runs in the same agent, NousResearch's Hermes
(`bench/harness/mtx_hermes.py`, pinned by `hermes` in the Justfile), on
the same terms: the same instruction, a shell, files, a todo list,
subagents and code execution, no web, the task's time limit, one
reasoning effort when the batch names one (`effort`; left out, each
harness keeps its own default), and no memory or skills from any other
trial.

## 1. The key

Put the provider's key on the bench machine, in `~/.machine-trading.env`
(or the repo's `.env`), never in a task, an image or a commit:

| Provider | Variable | `api` | `upstream` |
|---|---|---|---|
| OpenAI | `OPENAI_API_KEY` | `openai` | `https://api.openai.com/v1` |
| Anthropic | `ANTHROPIC_API_KEY` | `anthropic` | `https://api.anthropic.com` |
| Google (Gemini API) | `GEMINI_API_KEY` | `chat` | `https://generativelanguage.googleapis.com/v1beta/openai` |
| OpenRouter (Meta, Kimi, GLM, NVIDIA, Inkling, ...) | `OPENROUTER_API_KEY` | `chat`, `reasoning = "openrouter"` | `https://openrouter.ai/api/v1` |
| Any OpenAI-compatible host (Together, Fireworks, a lab's own API) | any name, e.g. `TOGETHER_API_KEY` | `chat` | the host's API root |

The keys never enter a trial. `just bench` starts a proxy on the bench
machine for the batch (`key_proxy.py`) that holds them, listening on the
docker bridge only. Any process in a container can read the others'
environments under `/proc`, so a key in the container would be a key the
agent could print into its trajectory. Instead each model (route) gets
its own token, and Hermes, in the trial's container, calls the proxy with
it; the proxy puts the key on the request it forwards. Harbor runs with
neither the keys nor the batch's secret in its environment.

A token is readable by the agent, so it is limited to what the agent may
do anyway: it opens its own route only, for the one model-call path
(`/responses`, `/v1/messages` or `/chat/completions`) and the route's own
model (any other `model` is refused), with only content-type, accept,
anthropic-version and feature `anthropic-beta` headers forwarded, and a
body limited to what Hermes sends for one answer (`body_policy.py`: an
allowlist of fields per API, the model's own tools only, no background or
stored responses, billing and fallback fields dropped, re-serialised from
the parsed JSON). It can
spend on that model while the batch runs; it can't reach another model,
another endpoint (files, batches, fine-tuning, admin, listings), another
account or project, or the proxy from outside the machine.

After the batch, however it ends, `just bench` searches every file of the
batch for every key, raw, URL-encoded and base64: a clean search writes
`.scan-passed`, which `just publish` requires; a hit, or a file it can't
read, quarantines the batch (`.quarantined` lists them): rotate the key
and don't share the batch.

Models in their native harness (`native:<model>`, Claude Code and Codex)
still take their key inside the container, as before; the search covers
their files too.

## 2. The model

One table per model in `bench/harness/models.toml`, by the name you'll
pass to `just bench`:

```toml
[kimi-k3]
api = "chat"
upstream = "https://openrouter.ai/api/v1"
reasoning = "openrouter"
model = "moonshotai/kimi-k3"
key = "OPENROUTER_API_KEY"

[llama-on-together]
api = "chat"
upstream = "https://api.together.xyz/v1"
model = "meta-llama/<model>"
key = "TOGETHER_API_KEY"
```

A name not in the file still works as before: `openai/<model>`, or a bare
name for Anthropic's.

## 3. Check it

```sh
just preflight "kimi-k3 gemini-3-1-pro"
```

One request of one token per model: it fails on a missing key, a refused
key or a model name the provider doesn't know, and prints nothing of the
key. `just bench` runs it for its Hermes models before the first trial
(not for `native:` models).

## 4. Run it

```sh
just bench "kimi-k3 glm-5-3" 5 3 0.12.0 1 0 high   # 5 trials per task, 3 at a time, high effort
just bench "inkling" 1 1 0.12.0 1 0 high
```

Arguments are positional: models, trials per task, trials at a time,
dataset version, time limit multiplier, `together` (1 runs the models at
once), effort. Our cross-family runs pass `high` explicitly; left empty,
no harness gets `--effort` and each runs at its own default (Hermes's is
medium), as releases up to now did. Results land in `jobs/bench/<batch>/`; read them with
`just review jobs/bench/<batch>`. A trial's `agent/` folder holds Hermes's
output (`hermes.txt`), its session (`hermes-session.jsonl`), the
trajectory (`trajectory.json`) and what Hermes's home held when the trial
started and ended (`hermes-home-start.txt`, empty, and
`hermes-home-end.txt`).

To run one task outside a batch, start the proxy yourself:

```sh
export MTX_PROXY_SECRET="$(python3 -I -c 'import secrets; print(secrets.token_hex(32))')"
python3 -I bench/harness/key_proxy.py --host "$(ip -4 -o addr show docker0 | awk '{print $4}' | cut -d/ -f1)" kimi-k3 > /tmp/proxy.url &
MTX_PROXY_TOKEN="$(python3 -I bench/harness/models.py token kimi-k3)" \
env -u OPENROUTER_API_KEY -u MTX_PROXY_SECRET PYTHONPATH=bench/harness harbor run -p bench/tasks/flash-crash-spcx -a mtx_hermes:Hermes \
  $(python3 -I bench/harness/models.py args kimi-k3 "$(cat /tmp/proxy.url)") --effort high --ak version=v0.23.1 -k 1 -n 1
```

The adapter refuses to run without the proxy (`--base-url` and
`--api-key-name MTX_PROXY_TOKEN`).

## The harness check

```sh
just hermes-check flash-crash-spcx          # every route; or: just hermes-check flash-crash-spcx openai
```

First the proxy under attack (`proxy_attacks.py`): another route's
token, every other path (files, batches, fine-tuning, admin, traversal,
encodings, queries), another model, account headers, the machine's
network address, a key in an error body, the scan's encodings and
unreadable files, a stream cut mid-answer and a chunked body. Then two
trials per route (`openai`, `anthropic`, `openrouter`, `chat`), one
after the other, through the key proxy to a stand-in provider on the
machine (`bench/checks/hermes`), with no model and no real key: it checks
the install, the route, the tools offered, the effort as each API takes
it, the trajectory and token counts, that the second trial starts with
nothing the first planted (a skill, a memory, files), and that the
provider's key is in no trial file, including a copy of every process
environment the agent could read under `/proc` (which does hold the
route's token, as a control). It lists what Hermes asked the proxy for
beyond its model calls (all refused). Run it after bumping `hermes` or Harbor.

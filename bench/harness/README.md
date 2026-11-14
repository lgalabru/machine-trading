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
machine for the batch (`key_proxy.py`) that holds them; Hermes, in the
trial's container, talks to the proxy with a token drawn for the batch,
and the proxy puts the key on each request it forwards. Any process in a
container can read the others' environments under `/proc`, so a key in
the container would be a key the agent could print into its trajectory.
Harbor itself runs without the keys in its environment. After the
batch, `just bench` searches every file of the batch for every key's
value; a hit quarantines the batch (`.quarantined` lists the files,
`just publish` refuses it): rotate the key and don't share the batch.

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
export MTX_PROXY_TOKEN="$(python3 -I -c 'import secrets; print(secrets.token_hex(32))')"
python3 -I bench/harness/key_proxy.py --host "$(hostname -I | awk '{print $1}')" kimi-k3 > /tmp/proxy.url &
env -u OPENROUTER_API_KEY PYTHONPATH=bench/harness harbor run -p bench/tasks/flash-crash-spcx -a mtx_hermes:Hermes \
  $(python3 -I bench/harness/models.py args kimi-k3 "$(cat /tmp/proxy.url)") --effort high --ak version=v0.23.1 -k 1 -n 1
```

The adapter refuses to run without the proxy (`--base-url` and
`--api-key-name MTX_PROXY_TOKEN`).

## The harness check

```sh
just hermes-check flash-crash-spcx          # every route; or: just hermes-check flash-crash-spcx openai
```

Two trials per route (`openai`, `anthropic`, `openrouter`, `chat`), one
after the other, through the key proxy to a stand-in provider on the
machine (`bench/checks/hermes`), with no model and no real key: it checks
the install, the route, the tools offered, the effort as each API takes
it, the trajectory and token counts, that the second trial starts with
nothing the first planted (a skill, a memory, files), and that the
provider's key is in no trial file, including a copy of every process
environment the agent could read under `/proc` (which does hold the
proxy's token, as a control). Run it after bumping `hermes` or Harbor.

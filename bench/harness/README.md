# Running a model in Hermes

Every model runs in the same agent, NousResearch's Hermes
(`bench/harness/mtx_hermes.py`, pinned by `hermes` in the Justfile), on
the same terms: the same instruction, a shell, files, a todo list,
subagents and code execution, no web, the task's time limit, one
reasoning effort (`effort`, `high` by default), and no memory or skills
from any other trial.

## 1. The key

Put the provider's key on the bench machine, in `~/.machine-trading.env`
(or the repo's `.env`), never in a task, an image or a commit:

| Provider | Variable | Route in models.toml |
|---|---|---|
| OpenAI | `OPENAI_API_KEY` | `openai/<model>` |
| Anthropic | `ANTHROPIC_API_KEY` | `anthropic/<model>` |
| Google (Gemini API) | `GEMINI_API_KEY` | `google/<model>`, with Gemini's OpenAI-compatible `base_url` |
| OpenRouter (Meta, Kimi, GLM, NVIDIA, Inkling, ...) | `OPENROUTER_API_KEY` | `openrouter/<vendor>/<model>` |
| Any OpenAI-compatible host (Together, Fireworks, a lab's own API) | any name, e.g. `TOGETHER_API_KEY` | `<host>/<model>`, with `base_url` and `api_format = "openai_chat_completions"` |

Harbor hands the key to Hermes as an environment variable of each command
it runs in the trial's container; it isn't written to the image, Hermes's
config, the trial's logs or its trajectory (`just hermes-check` checks the
last three).

## 2. The model

One table per model in `bench/harness/models.toml`, by the name you'll
pass to `just bench`:

```toml
[kimi-k3]
model = "openrouter/moonshotai/kimi-k3"
key = "OPENROUTER_API_KEY"

[llama-on-together]
model = "together/meta-llama/<model>"
key = "TOGETHER_API_KEY"
base_url = "https://api.together.xyz/v1"
api_format = "openai_chat_completions"
```

A name not in the file still works as before: `openai/<model>`, or a bare
name for Anthropic's.

## 3. Check it

```sh
just preflight "kimi-k3 gemini-3-1-pro"
```

One request of one token per model: it fails on a missing key, a refused
key or a model name the provider doesn't know, and prints nothing of the
key. `just bench` runs it too before its first trial.

## 4. Run it

```sh
just bench "kimi-k3 glm-5-3" 5 3 0.12.0            # 5 trials per task, 3 at a time
just bench "inkling" 1 1 0.12.0 1 0 medium          # ..., limit, together, effort
```

Arguments are positional: models, trials per task, trials at a time,
dataset version, time limit multiplier, `together` (1 runs the models at
once), effort. Results land in `jobs/bench/<batch>/`; read them with
`just review jobs/bench/<batch>`. A trial's `agent/` folder holds Hermes's
output (`hermes.txt`), its session (`hermes-session.jsonl`), the
trajectory (`trajectory.json`) and what Hermes's home held when the trial
started and ended (`hermes-home-start.txt`, empty, and
`hermes-home-end.txt`).

To run one task outside a batch:

```sh
PYTHONPATH=bench/harness harbor run -p bench/tasks/flash-crash-spcx -a mtx_hermes:Hermes \
  $(python3 -I bench/harness/models.py args kimi-k3) --effort high --ak version=v0.23.1 -k 1 -n 1
```

## The harness check

```sh
just hermes-check flash-crash-spcx
```

Two trials, one after the other, against a stand-in provider on the
machine (`bench/checks/hermes`), with no model and no key: it checks the
install, the provider route, the tools offered, the effort sent, the
trajectory and token counts, that the second trial starts with nothing the
first planted (a skill, a memory, files), and that the key reaches no
trial file. Run it after bumping `hermes` or Harbor.

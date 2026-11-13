# 0002. GitHub reaches the bench machine over SSH, through one command

- **Status:** Accepted (2026-11-13).
- **Scope:** `.github/workflows/bench.yml`, `ops/sunburst/`.

## Context

A bench runs for hours on a large machine, sunburst, and spends API
credits. We want to start, follow and publish one from GitHub Actions.
GitHub-hosted runners stop a job after six hours and are too small to grade
40 trials, so the work has to run on the machine. Two ways to get it there:

1. **A self-hosted runner on the machine.** GitHub advises against them on
   public repositories: a workflow on any branch, or a pull request from a
   fork if the settings ever allow it, runs its code on the machine, next
   to the API keys.
2. **SSH from a GitHub-hosted runner.** The job holds a key; the machine
   decides what that key may do.

## Decision

We use SSH, and each key may run one command: `~/bin/bench-dispatch`
(`restrict,command="…"` in `authorized_keys`, so no shell, no forwarding,
no terminal). The dispatcher parses `SSH_ORIGINAL_COMMAND` itself and
accepts `start`, `status` and `publish`, each argument checked against a
pattern. It runs only commits on `origin/main` and keeps a dirty checkout
untouched. A second key, `--read-only`, may only ask for `status`.

The keys live in GitHub environments that deploy from `main` alone, so a
workflow edited on a branch never sees them. `sunburst`, the key that
starts and publishes, waits for a reviewer. The machine's host key is
pinned in the environment, and the API keys never leave the machine:
`publish` uploads to the results dataset from there and hands back only
`release.json`.

## Consequences

- A leaked key can start a bench on main's code or publish a finished one,
  nothing else. It can't read a file or run a command.
- The dispatcher is a copy outside the checkout. Changing it means
  reinstalling it by hand, which is the point: a merged commit can't widen
  what GitHub may run.
- GitHub learns about a batch only when asked (`status`). Nothing on the
  machine can write to the repository.

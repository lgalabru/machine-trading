#!/usr/bin/env bash
# Leak probe, run as a bot: the grader derives the sessions' keypairs in memory
# from a secret it alone holds (MTX_EVAL_SEED), so nothing it signs with is on
# disk. A bot runs in the same container, as an unprivileged user; this one
# looks for what could still reach it during a session and reports each
# surface in its log, one `probe: surface=<name> status=<clear|LEAK|absent>`
# line each, then `probe: surfaces=N leaks=M`. It never prints a secret, and
# it tests the seed's presence only.
#   environ   no other process's environment is readable, and none shows MTX_EVAL_SEED
#   verifier  /tests, /solution and /logs/verifier hold nothing the bot can read
#   graderdirs  no readable /tmp/mtx-* directory but the bot's own (informed logs, other sessions)
#   keyfiles  no readable file on disk shaped like a keypair JSON (a 64-number array)
# The bot's own HOME is exempt from graderdirs and keyfiles. PROBE_PROC, PROBE_TMP, PROBE_VERIFIER and PROBE_KEYDIRS point the surfaces
# elsewhere, and PROBE_ONCE=1 checks once and exits (for the harness test).
set -u
proc="${PROBE_PROC:-/proc}"
tmp="${PROBE_TMP:-/tmp}"
verifier="${PROBE_VERIFIER:-/tests /solution /logs/verifier}"
keydirs="${PROBE_KEYDIRS:-/tmp /var/tmp /root /home /opt /app /srv /run /mnt /workspace}"
me="$(id -u)"
# The bot's own session directory is its HOME and holds its own keypair, which
# it is meant to read; the snapshot cache is public data.
own="${HOME:-/nonexistent-home}"
is_own() { case "$1" in "$own" | "$own"/*) return 0 ;; esac; return 1; }
leaks=""
note() { leaks="$leaks $1"; echo "probe: LEAK $1 $2"; }

check_environ() {
  local d pid f n=0
  for d in "$proc"/[0-9]*; do
    [ -d "$d" ] || continue
    pid="${d##*/}"
    [ "$pid" = "$$" ] && continue
    f="$d/environ"
    [ -e "$f" ] || continue
    [ -r "$f" ] && head -c 1 "$f" > /dev/null 2>&1 || continue
    n=$((n + 1))
    if tr '\0' '\n' < "$f" 2> /dev/null | grep -q '^MTX_EVAL_SEED='; then
      note environ "pid=$pid shows MTX_EVAL_SEED (value not printed)"
    elif [ "$(stat -c %u "$d" 2> /dev/null || echo "$me")" != "$me" ]; then
      note environ "pid=$pid owned by another user is readable"
    fi
  done
  environ_n="$n"
}

check_verifier() {
  local p present=0
  for p in $verifier; do
    [ -e "$p" ] || continue
    present=$((present + 1))
    if ls "$p" > /dev/null 2>&1 || head -c 1 "$p" > /dev/null 2>&1; then
      note verifier "$p is readable"
    fi
  done
  verifier_n="$present"
}

check_graderdirs() {
  local d present=0
  for d in "$tmp"/mtx-*; do
    [ -e "$d" ] || continue
    is_own "$d" && continue
    [ "${d##*/}" = mtx-snapshots ] && continue
    present=$((present + 1))
    if ls "$d" > /dev/null 2>&1; then
      note graderdirs "$d is readable"
    fi
  done
  graderdirs_n="$present"
}

check_keyfiles() {
  local f n=0 dirs=""
  for d in $keydirs; do [ -d "$d" ] && dirs="$dirs $d"; done
  [ -n "$dirs" ] || { keyfiles_n=0; return; }
  # shellcheck disable=SC2086
  while IFS= read -r f; do
    is_own "$f" && continue
    n=$((n + 1))
    if head -c 600 "$f" 2> /dev/null | tr -d ' \n' | grep -qE '^\[[0-9]{1,3}(,[0-9]{1,3}){63}'; then
      note keyfiles "$f is a readable keypair file"
    fi
  done < <(find $dirs -xdev -maxdepth 4 -type f -name '*.json' -size -4k 2> /dev/null)
  keyfiles_n="$n"
}

for round in 1 2 3 4; do
  environ_n=0 verifier_n=0 graderdirs_n=0 keyfiles_n=0
  check_environ
  check_verifier
  check_graderdirs
  check_keyfiles
  [ -n "${PROBE_ONCE:-}" ] && break
  sleep 0.5
done
total=0
for s in environ verifier graderdirs keyfiles; do
  case " $leaks " in
    *" $s "*) status=LEAK ;;
    *) status=clear ;;
  esac
  eval "n=\$${s}_n"
  [ "$status" = clear ] && [ "$n" -eq 0 ] && [ "$s" != environ ] && [ "$s" != keyfiles ] && status=absent
  echo "probe: surface=$s status=$status checked=$n"
  total=$((total + 1))
done
count=0
for l in $leaks; do count=$((count + 1)); done
echo "probe: surfaces=$total leaks=$count"
[ -n "${PROBE_ONCE:-}" ] && exit 0
# Sit out the rest of the session without trading.
trap 'exit 0' TERM
while true; do sleep 1; done

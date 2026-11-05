"""Terminal presentation for the frozen scenario coordinator. --follow is read-only."""
import argparse
from contextlib import redirect_stdout
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading
import time

import evaluate_set


def json_file(path):
    try:
        return json.loads(path.read_bytes())
    except (FileNotFoundError, ValueError):
        # The coordinator and Harbor update their reports while we read them.
        return {}


def scenario_names(scenarios):
    return {s['id']: s['regime'] for s in scenarios if s.get('regime')}


def named_rewards(rewards, names):
    return {names.get(key, key): value for key, value in rewards.items()}


def finalize_report(path):
    """Give the operator's summary named scores. Harbor trial records keep their IDs."""
    report = json_file(path)
    if report.get('status') not in ('complete', 'incomplete', 'disqualified'):
        return
    rewards = named_rewards(report['reward_by_scenario'], scenario_names(report['scenarios']))
    if rewards == report['reward_by_scenario']:
        return
    report['reward_by_scenario'] = rewards
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as tmp:
        try:
            json.dump(report, tmp, indent=2)
            tmp.write('\n')
            tmp.flush()
            os.replace(tmp.name, path)
        finally:
            Path(tmp.name).unlink(missing_ok=True)


class ScenarioConsole:
    """Render the frozen coordinator's operator messages using release names."""
    def __init__(self, stream, names):
        self.stream = stream
        self.names = names

    def write(self, message):
        if message.startswith('{'):
            payload = json.loads(message)
            if 'reward_by_scenario' in payload:
                payload['reward_by_scenario'] = named_rewards(payload['reward_by_scenario'], self.names)
                message = json.dumps(payload, indent=2)
        else:
            sid, sep, details = message.partition(': ')
            if sep and sid in self.names:
                message = self.names[sid] + sep + details
        return self.stream.write(message)

    def flush(self):
        self.stream.flush()


def docker(*args):
    try:
        result = subprocess.run(
            ['docker', *args], capture_output=True, timeout=2, check=False,
        )
        return result.stdout if result.returncode == 0 else b''
    except (OSError, subprocess.TimeoutExpired):
        return b''


def latest_activity(data):
    # Only summarize public tool activity. Commands, tool output, credentials
    # and model reasoning do not belong in terminal progress messages.
    for line in reversed(data.splitlines()):
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if not isinstance(event, dict):
            continue
        item = event.get('item') or {}
        if not isinstance(item, dict):
            continue
        kind = item.get('type')
        if kind == 'command_execution':
            text = item.get('command', '')
            action = re.search(r'\bcargo (build|check|test|run|build-sbf)\b', str(text))
            command = f'cargo {action[1]}' if action else 'shell command'
            if event.get('type') == 'item.started':
                return item.get('id'), f'started {command}'
            if event.get('type') == 'item.completed':
                code = item.get('exit_code')
                return item.get('id'), f'finished {command}' + (f' (exit {code})' if type(code) is int else '')
        if kind == 'file_change':
            return item.get('id'), 'editing project files'
        if kind == 'agent_message':
            return item.get('id'), 'posted a development update'
        if event.get('type') == 'turn.started':
            return None, 'started a development turn'
    return None, 'development event log available'


class Display:
    def __init__(self, root, follow, names):
        self.names = names
        self.root = root
        self.follow = follow
        # Existing attempts may be large and already scored when resuming.
        self.offsets = {p: p.stat().st_size for p in root.glob('scenario-*.log')}
        self.last_state = None
        self.last_print = self.next_probe = 0

    def update(self, final=False):
        report = json_file(self.root/'report.json')
        for sid, attempts in report.get('attempts', {}).items():
            for attempt in attempts:
                path = self.root/(Path(attempt).name + '.log')
                try:
                    with path.open('rb') as log:
                        # When attaching, start with current activity.
                        start = self.offsets.get(path, 0)
                        log.seek(start)
                        chunk = log.read(256 * 1024 if not final else -1)
                        self.offsets[path] = log.tell()
                except FileNotFoundError:
                    continue
                if chunk:
                    sys.stderr.buffer.write(chunk)
                    sys.stderr.buffer.flush()
        status = report.get('status')
        if status in ('complete', 'incomplete', 'disqualified'):
            if self.follow:
                rewards = named_rewards(report.get('reward_by_scenario', {}), scenario_names(report.get('scenarios', [])))
                print(f"Job {status}: {rewards}", flush=True)
            return True
        now = time.monotonic()
        attempts = report.get('attempts', {})
        if not attempts or now < self.next_probe or final:
            return False
        self.next_probe = now + 5
        sid = next(reversed(attempts))
        if not attempts[sid]:
            return False
        job = self.root/attempts[sid][-1]
        trial = next(job.glob('*/trial.log'), None)
        phase, event = 'Harbor setup', None
        if trial:
            # Harbor names its isolated Compose projects after the exact trial.
            slug = trial.parent.name.lower()
            names = docker('ps', '--filter', f'name=^/{re.escape(slug)}__',
                           '--format', '{{.Names}}').decode().splitlines()
            development = slug + '__env-main-1'
            if any(n.startswith(slug + '__verifier') for n in names):
                phase = 'Harbor verifier running'
            elif development in names:
                if sid == 'scenario-01' and len(attempts[sid]) == 1 and report.get('model'):
                    data = docker('exec', development, 'tail', '-c', '65536', '/logs/agent/codex.txt')
                    phase = 'preparing development environment'
                    if data:
                        event, activity = latest_activity(data)
                        phase = f"{report['model']} developing · {activity}"
                else:
                    phase = 'preparing submitted bot for evaluation'
        state = sid, phase, event
        if state != self.last_state or now - self.last_print >= 30:
            label = self.names.get(sid, sid)
            print(f'[{label}] {phase}', file=sys.stderr, flush=True)
            self.last_print, self.last_state = now, state
        return False


def main():
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument('--jobs-dir', type=Path, required=True)
    parser.add_argument('--job-name', required=True)
    parser.add_argument('--follow', action='store_true')
    parser.add_argument('--release', type=Path)
    args, _ = parser.parse_known_args()
    root = args.jobs_dir/args.job_name
    manifest = json_file(args.release/'release.json') if args.release else {}
    names = scenario_names(manifest.get('scenarios', []))
    display = Display(root, args.follow, names)
    if args.follow:
        if not (root/'report.json').is_file():
            raise SystemExit(f'No existing evaluation report at {root}/report.json')
        print(f'Following {root} (Ctrl-C disconnects)', flush=True)
        try:
            while not display.update():
                time.sleep(.25)
        except KeyboardInterrupt:
            pass
        return
    if '--dry-run' in sys.argv or root.exists() and '--resume' not in sys.argv:
        evaluate_set.main()
        return
    if sys.stderr.isatty():
        # Harbor uses Rich. Its stdout goes to the retained scenario log.
        # Render live frames there and mirror those frames to the terminal.
        os.environ.update(TTY_COMPATIBLE='1', TTY_INTERACTIVE='1')
        os.environ['COLUMNS'] = str(os.get_terminal_size(sys.stderr.fileno()).columns)
    stop = threading.Event()

    def monitor():
        while not stop.wait(.25):
            display.update()
    thread = threading.Thread(target=monitor, daemon=True)
    thread.start()
    try:
        with redirect_stdout(ScenarioConsole(sys.stdout, names)):
            evaluate_set.main()
    finally:
        stop.set()
        thread.join()
        display.update(final=True)
        finalize_report(root/'report.json')


if __name__ == '__main__':
    main()

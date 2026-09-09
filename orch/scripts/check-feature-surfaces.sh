#!/bin/sh
# Standalone delivery gate; never nest this complete build sequence in testExclusive.
set -eu
[ "$#" -eq 1 ] || { echo 'usage: check-feature-surfaces.sh /absolute/path/to/cargo' >&2; exit 64; }
case "$1" in /*) ;; *) echo 'cargo path must be absolute' >&2; exit 64 ;; esac
script_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
exec python3 - "$script_root" "$1" <<'PY'
import json, os, subprocess, sys, time
from pathlib import Path
root, cargo = Path(sys.argv[1]), sys.argv[2]
os.chdir(root)
head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
common = ['--locked', '--manifest-path', 'orch/Cargo.toml']
expected_stages = ['default-check', 'default-build', 'default-contract', 'default-dependencies',
                   'selfhost-check', 'selfhost-build', 'selfhost-contract', 'ui-check', 'ui-test',
                   'restore-selfhost', 'restored-contract']
completed = []
successful_commands = set()

def run(stage, argv, capture=False):
    start = time.monotonic()
    result = subprocess.run(argv, text=True, stdout=subprocess.PIPE if capture else None)
    packet = {'kind': 'command-result', 'root': str(root), 'head': head, 'previousStage': completed[-1] if completed else None,
              'stage': stage, 'argv': argv, 'exitCode': result.returncode,
              'elapsedSeconds': time.monotonic() - start}
    print(json.dumps(packet, ensure_ascii=False), flush=True)
    if result.returncode:
        if capture: print(result.stdout, file=sys.stderr)
        raise SystemExit(result.returncode)
    successful_commands.add(stage)
    return result.stdout or ''

def finish(stage):
    assert stage in successful_commands, f"stage has no successful command receipt: {stage}"
    assert expected_stages[len(completed)] == stage, (completed, stage)
    previous = completed[-1] if completed else None
    completed.append(stage)
    following = expected_stages[len(completed)] if len(completed) < len(expected_stages) else None
    print(json.dumps({'kind': 'stage-completed', 'head': head, 'previousStage': previous,
                      'stage': stage, 'nextStage': following}), flush=True)

def build(stage, selfhost):
    args = [cargo, 'build', '-p', 'orch-cli', '--no-default-features', *common]
    if selfhost: args += ['--features', 'selfhost']
    lines = run(stage, [*args, '--message-format=json'], True).splitlines()
    artifacts = []
    for line in lines:
        try: value = json.loads(line)
        except ValueError: continue
        if value.get('reason') == 'compiler-artifact': artifacts.append(value)
        elif value.get('reason') == 'compiler-message':
            print(value.get('message', {}).get('rendered', ''), file=sys.stderr, end='')
    binaries = [a['executable'] for a in artifacts if a['target']['name'] == 'orch' and a.get('executable')]
    hosts = [a for a in artifacts if a['target']['name'] == 'orch_host' and not a['profile']['test']]
    assert len(binaries) == len(hosts) == 1, 'missing/ambiguous actual Cargo artifact'
    assert ('selfhost' in hosts[0]['features']) == selfhost, hosts[0]['features']
    finish(stage)
    return binaries[0]

def command_names(text):
    rows, scanning = [], False
    for line in text.splitlines():
        if line == 'Commands:': scanning = True; continue
        if scanning and line == 'Options:': break
        if scanning and line.strip(): rows.append(line.split()[0])
    return sorted(rows)

def contract(stage, binary, selfhost):
    top = ['consult', 'doctor', 'guide', 'harness', 'wake']
    if selfhost: top += ['await-report', 'check', 'cost', 'current', 'dispatch', 'ledger', 'plan',
                        'review', 'round', 'seal', 'sites', 'snapshot', 'stall-check', 'status', 'verdict']
    assert command_names(run(stage, [binary, '--help'], True)) == sorted(top)
    groups = {'harness': ['lint', 'list']}
    if selfhost: groups.update(review=['deliver'], round=['close', 'open', 'seed-verified', 'sign-off'],
                              ledger=['recover'], sites=['cache', 'gc', 'sweep-scratch', 'sweep-targets', 'sweep-trial-cache'])
    if selfhost: groups['sites cache'] = ['run', 'status', 'sweep']
    for name, leaves in groups.items():
        assert command_names(run(stage, [binary, *name.split(), '--help'], True)) == leaves
    if selfhost:
        assert '--dry-run' in run(stage, [binary, 'sites', 'gc', '--help'], True)
        assert '--last-maintenance' in run(stage, [binary, 'sites', 'cache', 'status', '--help'], True)
    text = run(stage, [binary, 'guide', '--check'], True)
    assert f'commands={30 if selfhost else 6} ' in text, text
    print(text.strip(), flush=True)
    finish(stage)

run('default-check', [cargo, 'check', '-p', 'orch-cli', '--no-default-features', *common]); finish('default-check')
binary = build('default-build', False)
contract('default-contract', binary, False)
tree = run('default-dependencies', [cargo, 'tree', '-p', 'orch-cli', '--no-default-features', *common,
           '--edges', 'normal', '--prefix', 'none', '--format', '{p}|{f}'], True)
rows = tree.splitlines()
for name in ['orch-ui', 'ratatui', 'crossterm', 'axum', 'tokio']:
    assert not any(line.startswith(name + ' v') for line in rows), f'default dependency leaked: {name}'
hosts = [line for line in rows if line.startswith('orch-host v')]
assert len(hosts) == 1 and 'selfhost' not in hosts[0].split('|')[1].replace(' ', '').split(','), hosts
print(json.dumps({'stage': 'default-dependencies', 'rows': len(rows), 'host': hosts[0]}), flush=True)
finish('default-dependencies')
run('selfhost-check', [cargo, 'check', '-p', 'orch-cli', '--no-default-features', '--features', 'selfhost', *common]); finish('selfhost-check')
binary = build('selfhost-build', True)
contract('selfhost-contract', binary, True)
run('ui-check', [cargo, 'check', '-p', 'orch-ui', *common]); finish('ui-check')
run('ui-test', [cargo, 'test', '-p', 'orch-ui', *common]); finish('ui-test')
binary = build('restore-selfhost', True)
contract('restored-contract', binary, True)
assert completed == expected_stages, completed
assert subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip() == head, 'HEAD moved during delivery gate'
print(json.dumps({'gate': 'feature-surfaces', 'head': head, 'completedStages': completed,
                  'status': 'passed', 'selfhostExecutable': binary}), flush=True)
PY

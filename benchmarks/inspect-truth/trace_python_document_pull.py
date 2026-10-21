#!/usr/bin/env python3
"""Capture real dynamically registered Python document pulls without timing certificates."""
import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import time

p = argparse.ArgumentParser()
p.add_argument('--server', required=True)
p.add_argument('--pythonpath', required=True)
p.add_argument('--out', type=Path, required=True)
a = p.parse_args()
out = a.out.resolve()
root = out / 'project'
root.mkdir(parents=True, exist_ok=True)
(root / 'pyproject.toml').write_text("[project]\nname = 'pull-fixture'\nversion = '0.1.0'\n")
(root / 'pyrightconfig.json').write_text(json.dumps({'typeCheckingMode': 'basic', 'pythonVersion': '3.14'}))
f = root / 'broken.py'
f.write_text("value: int = 'wrong'\n")
q = queue.Queue()
frames = []
registrations = {}
lock = threading.Lock()
proc = subprocess.Popen([a.server, '--stdio'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=(out / 'stderr.log').open('wb'), cwd=root)

def record(direction, m):
    frame = {'t': time.time(), 'direction': direction, 'message': m}
    with lock:
        frames.append(frame)
        with (out / 'frames.jsonl').open('a') as log:
            log.write(json.dumps(frame) + '\n')

def send(m):
    data = json.dumps(m).encode()
    record('to_server', m)
    proc.stdin.write(f'Content-Length: {len(data)}\r\n\r\n'.encode() + data)
    proc.stdin.flush()

def reader():
    while True:
        n = None
        while True:
            line = proc.stdout.readline()
            if not line:
                q.put(None)
                return
            if line.lower().startswith(b'content-length:'):
                n = int(line.split(b':')[1])
            if line == b'\r\n':
                break
        m = json.loads(proc.stdout.read(n))
        record('from_server', m)
        q.put(m)

threading.Thread(target=reader, daemon=True).start()
def receive(timeout=30):
    m = q.get(timeout=timeout)
    if m is None: raise RuntimeError('server exited')
    if 'id' in m and 'method' in m:
        result = None
        if m['method'] == 'workspace/configuration':
            result = [{'pythonPath': a.pythonpath} if item.get('section') == 'python' else None for item in m['params']['items']]
        elif m['method'] == 'client/registerCapability':
            for r in m['params']['registrations']:
                if r['method'] == 'textDocument/diagnostic': registrations[r['id']] = r.get('registerOptions', {})
        elif m['method'] == 'client/unregisterCapability':
            for r in m['params'].get('unregisterations', m['params'].get('unregistrations', [])):
                registrations.pop(r['id'], None)
        send({'jsonrpc': '2.0', 'id': m['id'], 'result': result})
    return m

def response(rid):
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        m = receive(deadline - time.monotonic())
        if m.get('id') == rid and 'method' not in m: return m
    raise RuntimeError('response timeout')

def document_open():
    send({'jsonrpc': '2.0', 'method': 'textDocument/didOpen', 'params': {'textDocument': {'uri': f.as_uri(), 'languageId': 'python', 'version': 0, 'text': f.read_text()}}})

def pull(rid):
    assert registrations, 'never send an unregistered diagnostic request'
    identifier = next(iter(registrations.values())).get('identifier')
    send({'jsonrpc': '2.0', 'id': rid, 'method': 'textDocument/diagnostic', 'params': {'textDocument': {'uri': f.as_uri()}, 'identifier': identifier}})
    r = response(rid)
    assert 'error' not in r, r
    assert r['result']['kind'] == 'full', r
    errors = [d for d in r['result']['items'] if d.get('severity') == 1]
    assert len(errors) == 1, r
    return r

try:
    send({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'processId': os.getpid(), 'rootUri': root.as_uri(), 'capabilities': {'workspace': {'configuration': True, 'workspaceFolders': True, 'diagnostic': {'refreshSupport': True}}, 'window': {'workDoneProgress': True}, 'textDocument': {'diagnostic': {'dynamicRegistration': True, 'relatedDocumentSupport': True}, 'publishDiagnostics': {'versionSupport': True}}}, 'workspaceFolders': [{'uri': root.as_uri(), 'name': 'project'}]}})
    initialized = response(1)
    send({'jsonrpc': '2.0', 'method': 'initialized', 'params': {}})
    while not registrations: receive()
    before = len(frames)
    document_open()
    first = pull(2)
    send({'jsonrpc': '2.0', 'method': 'textDocument/didClose', 'params': {'textDocument': {'uri': f.as_uri()}}})
    # Observe exceptional close/tracking notifications. This wait is capture
    # instrumentation only; diagnostic authority comes from the pull responses.
    end = time.monotonic() + 2
    while time.monotonic() < end:
        try: receive(end - time.monotonic())
        except queue.Empty: break
    document_open()
    second = pull(3)
    selected = []
    for frame in frames[before:]:
        m = frame['message']; method = m.get('method')
        if method in ['textDocument/didOpen', 'textDocument/didClose', 'textDocument/diagnostic', 'textDocument/publishDiagnostics'] or (m.get('id') in [2,3] and 'result' in m):
            selected.append(frame)
            print(json.dumps(frame), flush=True)
    summary = {'initialize_diagnosticProvider': initialized['result']['capabilities'].get('diagnosticProvider'), 'registrations': registrations, 'first_errors': 1, 'reopen_errors': 1, 'pushes': [frame for frame in selected if frame['message'].get('method') == 'textDocument/publishDiagnostics']}
    (out / 'summary.json').write_text(json.dumps(summary, indent=2))
    print('REAL_PULL_SUMMARY', json.dumps(summary), flush=True)
finally:
    proc.terminate()
    proc.wait(timeout=10)

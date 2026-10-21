#!/usr/bin/env python3
"""Log Python JSON-RPC traffic; optionally inject an explicitly marked startup empty push."""
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

real = os.environ['AFT_REAL_PYTHON_SERVER']
if '--version' in sys.argv:
    raise SystemExit(subprocess.call([real, *sys.argv[1:]]))
child = subprocess.Popen([real, *sys.argv[1:]], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
log = Path(os.environ['AFT_REAL_PYTHON_TRACE'])
lock = threading.Lock()
output_lock = threading.Lock()
def relay(source, target, direction):
    while True:
        headers = []
        length = None
        while True:
            line = source.readline()
            if not line: return
            headers.append(line)
            if line.lower().startswith(b'content-length:'): length = int(line.split(b':')[1])
            if line == b'\r\n': break
        body = source.read(length)
        with lock, log.open('a') as f:
            f.write(json.dumps({'t': time.time(), 'direction': direction, 'message': json.loads(body)}) + '\n')
        with output_lock if direction == 'from_server' else threading.Lock():
            target.write(b''.join(headers) + body)
            target.flush()
        message = json.loads(body)
        if direction == 'to_server' and message.get('method') == 'textDocument/didOpen' and os.environ.get('AFT_INJECT_STARTUP_EMPTY_PUSH') == '1':
            doc = message['params']['textDocument']
            injected = {'jsonrpc': '2.0', 'method': 'textDocument/publishDiagnostics', 'params': {'uri': doc['uri'], 'version': doc['version'], 'diagnostics': []}}
            data = json.dumps(injected).encode()
            with output_lock:
                with lock, log.open('a') as f:
                    f.write(json.dumps({'t': time.time(), 'direction': 'injected_to_client', 'message': injected}) + '\n')
                sys.stdout.buffer.write(f'Content-Length: {len(data)}\r\n\r\n'.encode() + data)
                sys.stdout.buffer.flush()
threading.Thread(target=relay, args=(sys.stdin.buffer, child.stdin, 'to_server'), daemon=True).start()
relay(child.stdout, sys.stdout.buffer, 'from_server')

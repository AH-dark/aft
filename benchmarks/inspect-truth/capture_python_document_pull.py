#!/usr/bin/env python3
"""Capture an AFT scoped file certificate against an isolated real Python server."""
import argparse
import json
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run as truth

p = argparse.ArgumentParser()
p.add_argument('--aft-bin', type=Path, required=True)
p.add_argument('--server', type=Path, required=True)
p.add_argument('--out', type=Path, required=True)
p.add_argument('--inject-startup-empty', action='store_true')
a = p.parse_args()
out = a.out.resolve()
root = out / 'project'
root.mkdir(parents=True, exist_ok=True)
(root / 'pyproject.toml').write_text("[project]\nname = 'aft-pull-fixture'\nversion = '0.1.0'\n")
(root / 'pyrightconfig.json').write_text(json.dumps({'typeCheckingMode': 'basic', 'pythonVersion': '3.14'}))
(root / 'broken.py').write_text("value: int = 'wrong'\n")
(root / '.cortexkit').mkdir(exist_ok=True)
truth.write_json(root / '.cortexkit/aft.jsonc', {'indexes': {'semantic': False, 'trigram': False, 'callgraph': False}, 'inspect': {'enabled': True}})
bin_dir = root / 'node_modules/.bin'
bin_dir.mkdir(parents=True, exist_ok=True)
proxy = Path(__file__).with_name('trace_python_pull_proxy.py').resolve()
link = bin_dir / 'pyright-langserver'
link.unlink(missing_ok=True)
link.symlink_to(proxy)
os.environ['AFT_REAL_PYTHON_SERVER'] = str(a.server.resolve())
os.environ['AFT_REAL_PYTHON_TRACE'] = str(out / 'frames.jsonl')
if a.inject_startup_empty:
    os.environ['AFT_INJECT_STARTUP_EMPTY_PUSH'] = '1'
user = out / 'user.jsonc'
truth.write_json(user, {})
s = truth.AftSession(a.aft_bin.resolve(), root, out / 'storage')
try:
    configured = s.configure(user)
    assert configured['success'], configured
    hover = s.client.request('lsp_hover', file='broken.py', line=1, character=1, timeout_s=60)
    truth.write_json(out / 'hover.json', hover)
    r = s.inspect(['diagnostics'], scope=['broken.py'], timeout_s=180)
    truth.write_json(out / 'inspect.json', r)
    d = r['summary']['diagnostics']
    assert r['success'] and d['errors'] == 1 and d.get('complete', True), r
    assert d['by_producer']['python']['authority'][0]['rule'] == 'document_pull', r
    assert d['by_producer']['python']['authority'][0]['no_progress_grace_ms'] == 0, r
    print('REAL_AFT_PULL', json.dumps({'errors': d['errors'], 'coverage': d['coverage'], 'authority': d['by_producer']['python']['authority']}), flush=True)
    status = s.client.request('status', timeout_s=60)
    truth.write_json(out / 'status.json', status)
finally:
    s.close()

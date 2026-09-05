import argparse
import ctypes
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import tomllib

parser = argparse.ArgumentParser()
parser.add_argument('--launcher', type=Path, required=True)
args = parser.parse_args()
launcher = [str(args.launcher.resolve()), '--exact', 'kimi::tests::native_kimi_lease_survives_launcher_exit', '--ignored', '--nocapture', '--format', 'terse']
if os.name != 'nt':
    raise SystemExit('This probe requires Windows')
if not shutil.which('kimi'):
    raise SystemExit('Install native Kimi Code before running this probe')

kernel = ctypes.WinDLL('kernel32', use_last_error=True)
kernel.CreateFileW.argtypes = [ctypes.c_wchar_p, ctypes.c_uint32, ctypes.c_uint32,
                              ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32,
                              ctypes.c_void_p]
kernel.CreateFileW.restype = ctypes.c_void_p
kernel.CloseHandle.argtypes = [ctypes.c_void_p]


def writable(path):
    handle = kernel.CreateFileW(str(path), 0x40000000, 7, None, 3, 0, None)
    if handle == ctypes.c_void_p(-1).value:
        error = ctypes.get_last_error()
        assert error == 32, error
        return False
    kernel.CloseHandle(handle)
    return True


def read_replies(stream, replies):
    for line in stream:
        try:
            replies.put(json.loads(line))
        except ValueError:
            pass


def request(process, replies, ident, method, params):
    process.stdin.write(json.dumps(dict(jsonrpc='2.0', id=ident, method=method, params=params)) + '\n')
    process.stdin.flush()
    while True:
        reply = replies.get(timeout=60)
        if reply.get('id') == ident:
            assert 'error' not in reply, reply
            return reply['result']


def native_pid(parent):
    result = subprocess.run([
        'powershell', '-NoProfile', '-Command',
        f'Get-CimInstance Win32_Process -Filter "ParentProcessId = {parent}" | Select-Object -ExpandProperty ProcessId'
    ], capture_output=True, text=True, check=True, timeout=30)
    children = result.stdout.split()
    assert len(children) == 1, result.stdout
    return int(children[0])


def stop_native(pid):
    subprocess.run(['taskkill', '/PID', str(pid), '/T', '/F'], capture_output=True, timeout=30)


with tempfile.TemporaryDirectory(prefix='rx-kimi-native-') as temporary:
    root = Path(temporary)
    home = root / 'home'
    (home / '.recall').mkdir(parents=True)
    khome = home / '.kimi-code'
    khome.mkdir()
    config = khome / 'config.toml'
    marker = khome / 'config.toml.rx-catalog.json'
    config.write_text('telemetry=false\ndefault_model="rx-first/model-a"\n', encoding='utf-8')
    (home / '.recall/rx.toml').write_text(''.join(
        f'[provider.{provider}]\nbase_url="http://127.0.0.1:1"\nenv="TEST_KEY"\nauth="env"\n'
        for provider in ('first', 'second')
    ), encoding='utf-8')
    env = dict(os.environ, RX_TEST_KIMI_HOME=str(home), KIMI_CODE_HOME=str(khome),
               TEST_KEY='synthetic-only', RX_NO_INSTALL='1', RX_NO_UPDATE='1', RX_NO_YOLO='1',
               HTTP_PROXY='http://127.0.0.1:1', HTTPS_PROXY='http://127.0.0.1:1',
               NO_PROXY='127.0.0.1,localhost')
    parents = []
    children = []
    errors = []

    def seed(provider):
        result = subprocess.run(launcher,
                                env=dict(env, RX_TEST_KIMI_PROVIDER=provider), cwd=root, capture_output=True, text=True, timeout=60)
        assert result.returncode == 0, result.stderr
        return tomllib.loads(config.read_text(encoding='utf-8'))

    try:
        for index in range(2):
            stderr = open(root / f'native-{index}.stderr', 'w+', encoding='utf-8')
            errors.append(stderr)
            process = subprocess.Popen(launcher,
                                       env=dict(env, RX_TEST_KIMI_PROVIDER='first', RX_TEST_KIMI_ACP='1'), cwd=root, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                       stderr=stderr, text=True, encoding='utf-8')
            parents.append(process)
            replies = queue.Queue()
            threading.Thread(target=read_replies, args=(process.stdout, replies), daemon=True).start()
            initialized = request(process, replies, 1, 'initialize', dict(protocolVersion=1, clientCapabilities={}))
            session = request(process, replies, 2, 'session/new', dict(cwd=str(root), mcpServers=[]))
            options = next(option for option in session['configOptions'] if option['id'] == 'model')
            assert options['currentValue'] == 'rx-first/model-a', options
            children.append(native_pid(process.pid))
        records = json.loads(marker.read_text(encoding='utf-8'))['catalogs']
        assert len(records) == 1
        lease = khome / next(iter(records))
        assert len(list(khome.glob('.rx-kimi-*'))) == 1
        assert not writable(lease)
        for parent in parents:
            parent.kill()
            parent.wait(timeout=10)
        assert not writable(lease)
        assert 'rx-first/model-a' in seed('second')['models']
        stop_native(children[0])
        assert not writable(lease)
        assert 'rx-first/model-a' in seed('second')['models']
        stop_native(children[1])
        assert writable(lease)
        assert 'rx-first/model-a' not in seed('second')['models']
        for _ in range(3):
            seed('first')
            seed('second')
        assert len(list(khome.glob('.rx-kimi-*'))) == 2
        assert len(json.loads(marker.read_text(encoding='utf-8'))['catalogs']) == 1
        print(json.dumps(dict(native=initialized['agentInfo'], parents_terminated=True,
                              native_survived_parent=True, first_exit_retained=True,
                              last_exit_reclaimed=True, repeated_snapshot_files=2)))
    finally:
        for parent in parents:
            if parent.poll() is None:
                parent.kill()
                parent.wait(timeout=10)
        for child in children:
            stop_native(child)
        for stderr in errors:
            stderr.seek(0)
            print(stderr.read())
            stderr.close()

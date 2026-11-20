#!/usr/bin/env python3
"""Exercise private-pipe lifetime with an original fake runtime, never an account."""
import json
import ctypes
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time


class Timebase(ctypes.Structure):
    _fields_ = [('numer', ctypes.c_uint32), ('denom', ctypes.c_uint32)]


_system = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
_system.mach_continuous_time.argtypes = []
_system.mach_continuous_time.restype = ctypes.c_uint64
_system.mach_absolute_time.argtypes = []
_system.mach_absolute_time.restype = ctypes.c_uint64
_system.mach_timebase_info.argtypes = [ctypes.POINTER(Timebase)]
_system.mach_timebase_info.restype = ctypes.c_int
_timebase = Timebase()
assert _system.mach_timebase_info(ctypes.byref(_timebase)) == 0 and _timebase.numer and _timebase.denom


def continuous_ns():
    return _system.mach_continuous_time() * _timebase.numer // _timebase.denom


def uptime_ms():
    return _system.mach_absolute_time() * _timebase.numer // _timebase.denom // 1_000_000


def receive(process):
    assert select.select([process.stdout], [], [], 5)[0], "No bounded supervisor response"
    value = process.stdout.readline(48 * 1024 + 2)
    assert len(value) <= 48 * 1024 + 1, 'Unbounded supervisor frame'
    assert b"fixture-key" not in value, "Supervisor disclosed startup configuration"
    return json.loads(value)


def until(process, predicate):
    for _ in range(24):
        value = receive(process)
        if predicate(value):
            return value
    raise AssertionError("Expected bounded supervisor event was not emitted")


with tempfile.TemporaryDirectory(prefix="auvra-playback-fixture-") as directory:
    root = Path(directory)
    runtime = root / "runtime"
    fixture = Path(__file__).resolve().parents[1] / "platforms/macos/Tests/Fixtures/soloist-runtime.py"
    runtime.write_text(f"#!{sys.executable}\n" + fixture.read_text())
    runtime.chmod(0o700)
    helper = sys.argv[1]

    def launch(name, volume=None):
        process = subprocess.Popen([helper], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        request = dict(executable=str(runtime), dataDirectory=str(root / "data"), cacheDirectory=str(root / "cache"), deviceName=name, apiKey="fixture-key")
        if volume is not None:
            request['initialVolume'] = volume
        process.stdin.write(json.dumps(request).encode() + b"\n")
        process.stdin.flush()
        return process

    def bootstrap(process, id, action='activate', expires_at=None, continuous_deadline=None):
        request = dict(type='bootstrap', action=action, id=id,
                       expires_at_uptime_ms=uptime_ms() + 1500 if expires_at is None else expires_at,
                       expires_at_continuous_ns=continuous_ns() + 1_500_000_000 if continuous_deadline is None else continuous_deadline)
        process.stdin.write(json.dumps(request).encode() + b'\n')

    def activation_result(process, id):
        return until(process, lambda event: event.get('event') == 'activation_result' and event.get('id') == id)

    for mode in ('ack', 'wrong-ack', 'auth-loss', 'withheld', 'cancel', 'credit', 'forbidden', 'stale', 'unauthenticated'):
        fixture_mode = 'withheld' if mode == 'cancel' else mode
        process = launch('ws-bootstrap-' + fixture_mode)
        assert receive(process)['control_protocol'] == 1
        if mode == 'unauthenticated':
            until(process, lambda event: event.get('event') == 'auth' and event.get('logged_in') is False)
        else:
            initial = until(process, lambda event: bool(event.get('clock')))['clock']
            assert initial['active'] == (mode == 'credit')
        if mode == 'forbidden':
            # Wait for the cancel acknowledgement before submitting the held
            # request: its monotonic tombstone must reject a later dispatch.
            bootstrap(process, 1, action='cancel', expires_at=0)
            assert not activation_result(process, 1)['accepted']
            bootstrap(process, 1)
            assert not activation_result(process, 1)['accepted']
            bootstrap(process, 2, expires_at=uptime_ms() - 1)
            assert not activation_result(process, 2)['accepted']
            bootstrap(process, 3, expires_at=uptime_ms() + 10_000)
            assert not activation_result(process, 3)['accepted']
            bootstrap(process, 4, action='deactivate')
            assert not activation_result(process, 4)['accepted']
            bootstrap(process, 5, continuous_deadline=continuous_ns() - 1)
            assert not activation_result(process, 5)['accepted'], 'Fresh awake uptime revived a request that expired during sleep'
            bootstrap(process, 6, continuous_deadline=continuous_ns() + 10_000_000_000)
            assert not activation_result(process, 6)['accepted'], 'Unbounded continuous expiry was accepted'
            process.stdin.write(json.dumps(dict(type='bootstrap', action='activate', id=7,
                expires_at_uptime_ms=uptime_ms() + 1500)).encode() + b'\n')
            assert not activation_result(process, 7)['accepted'], 'Missing sleep-safe expiry was accepted'
            batch = [dict(type='bootstrap', action=action, id=8,
                          expires_at_uptime_ms=uptime_ms() + 1500,
                          expires_at_continuous_ns=continuous_ns() + 1_500_000_000)
                     for action in ('cancel', 'activate')]
            process.stdin.write(b''.join(json.dumps(request).encode() + b'\n' for request in batch))
            assert not activation_result(process, 8)['accepted']
            assert not activation_result(process, 8)['accepted'], 'An already-read cancellation raced queued activation'
        else:
            if mode == 'stale':
                time.sleep(1.05)
            started_at = time.monotonic()
            bootstrap(process, 1)
            if mode in ('cancel', 'credit'):
                until(process, lambda event: (event.get('clock') or {}).get('volume') == .23)
                if mode == 'credit':
                    bootstrap(process, 2)
                    assert not activation_result(process, 2)['accepted'], 'Two bootstrap writes overlapped'
                    process.stdin.write(json.dumps(dict(id=3, sequence=initial['sequence'],
                        media_id=initial['media_id'], expires_at_uptime_ms=uptime_ms() + 1500,
                        control=dict(kind='pause'))).encode() + b'\n')
                    assert not until(process, lambda event: event.get('event') == 'control_result' and event.get('id') == 3)['accepted'], 'Activation did not hold the native wire credit'
                bootstrap(process, 1, action='cancel', expires_at=0)
            result = activation_result(process, 1)
            assert result['accepted'] == (mode in ('ack', 'wrong-ack')), mode
            if mode == 'wrong-ack':
                assert time.monotonic() - started_at >= .12, 'An unrelated play ACK completed activation'
            if mode == 'withheld':
                assert 1.1 <= time.monotonic() - started_at < 2, 'Activation ACK timeout was not bounded'
        process.stdin.close()
        assert process.wait(timeout=5) == 0 and process.stderr.read(4096) == b'', mode

    clock_fixture = root / 'bootstrap-clock-fixtures'
    clock_source = Path(__file__).resolve().parents[1] / 'platforms/macos/Helpers/SoloistSupervisor.swift'
    compiled = subprocess.run(['xcrun', 'swiftc', '-O', '-D', 'AUVRA_BOOTSTRAP_CLOCK_FIXTURES',
        '-swift-version', '6', '-target', 'arm64-apple-macosx14.0', '-parse-as-library',
        str(clock_source), '-o', str(clock_fixture)], capture_output=True, timeout=60)
    assert compiled.returncode == 0 and not compiled.stderr, compiled.stderr.decode()
    subprocess.run([str(clock_fixture)], check=True, timeout=5)
    print('Spotify bootstrap helper fixtures passed: ACK identity, expiry, cancellation, auth, freshness and shared wire credit.')
    if '--bootstrap-only' in sys.argv[2:]:
        sys.exit(0)

    process = launch("Auvra fixture")
    started = receive(process)
    assert started["event"] == "started"
    pid = started["pid"]
    os.kill(pid, 0)
    process.stdin.close()  # The same EOF is delivered when Auvra crashes.
    assert process.wait(timeout=5) == 0
    try:
        os.kill(pid, 0)
        raise AssertionError("Owned runtime survived parent pipe closure")
    except ProcessLookupError:
        pass
    assert process.stderr.read(4096) == b""

    process = launch('volume-zero', 0)
    assert receive(process)['event'] == 'started'
    assert receive(process) == {'event': 'exited', 'code': 0}, 'Saved mute was not passed to the Soloist startup argument'
    assert process.wait(timeout=5) == 0
    process.stdin.close()
    process = launch('volume-zero', 101)
    assert process.wait(timeout=5) == 2 and process.stdout.read(1) == b''
    process.stdin.close()

    process = launch("expired")
    assert receive(process)["event"] == "started"
    assert receive(process) == {"event": "exited", "code": 10}
    assert process.wait(timeout=5) == 10
    process.stdin.close()

    process = launch("ws-fixture")
    assert receive(process)["event"] == "started"
    assert receive(process) == {"event": "auth", "logged_in": True}, "Native auth health did not cross the sanitized protocol"
    assert until(process, lambda v: v.get('event') == 'auth') == {"event": "auth", "logged_in": False}, "Malformed playback disclosed raw content"
    process.stdin.close()
    assert process.wait(timeout=5) == 0

    process = launch('ws-clock-fixture')
    started = receive(process)
    assert started['clock_protocol'] == 1
    values = []
    while True:
        event = receive(process)
        if event.get('clock'):
            values.append(event['clock'])
        if event == dict(event='auth', logged_in=False):
            break
    assert len(values) == 4, 'Invalid clock fields escaped the adapter'
    clock_fields = {'sequence', 'media_id', 'state', 'position_ms', 'rate', 'active', 'uptime', 'volume', 'options', 'actions', 'metadata', 'queue_epoch'}
    assert all(set(value) - {'source_position_timestamp_ms'} == clock_fields for value in values)
    assert all(type(value['source_position_timestamp_ms']) in (int, float) and value['source_position_timestamp_ms'] > 0
               for value in values[:2]), 'Native position anchors lost their source times'
    assert all('source_position_timestamp_ms' not in value for value in values[2:]), 'Synthetic position must not confirm a seek'
    assert all(value['media_id'] == 'spotify:track:' + 'a' * 22 for value in values)
    assert values[0]['rate'] == 1.5 and values[0]['state'] == 'playing'
    assert 4321 <= values[1]['position_ms'] < 5000
    assert values[2]['state'] == 'paused' and values[2]['rate'] == 0
    assert values[3]['active'] is False
    assert [value['sequence'] for value in values] == sorted({value['sequence'] for value in values})
    process.stdin.close()
    assert process.wait(timeout=5) == 0

    process = launch('ws-queue-fixture')
    assert receive(process)['event'] == 'started'
    first = until(process, lambda event: event.get('event') == 'queue')['queue']
    assert first['media_id'] == 'spotify:track:' + 'a' * 22
    assert len(first['previous']) == len(first['upcoming']) == 10 and len(json.dumps(first)) > 4096
    assert first['upcoming'][0]['media_id'] == first['upcoming'][1]['media_id']
    assert [row['source'] for row in first['upcoming'][:3]] == ['queue', 'autoplay', None]
    assert first['upcoming'][0]['metadata']['duration_ms'] == 0
    second = until(process, lambda event: event.get('event') == 'queue')['queue']
    assert second['media_id'] == 'spotify:track:' + 'b' * 22 and second['epoch'] > first['epoch']
    assert not second['upcoming'] and not second['previous'], 'Old rows crossed a track transition'
    process.stdin.close()
    assert process.wait(timeout=5) == 0

    process = launch('ws-metadata-fixture')
    assert receive(process)['event'] == 'started'
    values = []
    while True:
        event = receive(process)
        if event.get('clock'):
            values.append(event['clock'])
        if event == dict(event='auth', logged_in=False):
            break
    assert len(values) == 4
    assert values[0]['metadata'] == dict(title='Original title', artists=['Original artist'], album='Original album',
                                         duration_ms=65432, artwork='https://i.scdn.co/image/large', context_uri='spotify:album:' + 'b' * 22)
    assert values[1]['metadata']['context_uri'] == 'spotify:playlist:' + 'c' * 22
    assert values[2]['media_id'] == 'spotify:track:' + 'd' * 22
    assert values[2]['metadata'] is None and values[3]['metadata'] is None, 'New recording inherited stale metadata'
    process.stdin.close()
    assert process.wait(timeout=5) == 0

    process = launch('ws-track-transition-fixture')
    assert receive(process)['event'] == 'started'
    values = []
    while True:
        event = receive(process)
        if event.get('clock'):
            values.append(event['clock'])
        if event == dict(event='auth', logged_in=False):
            break
    assert len(values) == 7, 'The delayed previous-item snapshot must be ignored'
    first, refresh, changing, duplicate, sync, playing, confirmed = values
    assert first['media_id'] == refresh['media_id'] == 'spotify:track:' + 'a' * 22
    assert refresh['metadata']['title'] == 'Updated current track' and refresh['state'] == 'playing'
    assert refresh['rate'] == first['rate'] == 1.5 and refresh['position_ms'] >= first['position_ms']
    assert all(value['media_id'] == 'spotify:track:' + 'b' * 22 for value in values[2:])
    assert changing['metadata']['title'] == 'Original next track' and changing['metadata']['context_uri'] is None
    assert changing['metadata']['artwork'] == 'https://i.scdn.co/image/b' and changing['metadata']['duration_ms'] == 65000
    assert changing['position_ms'] == duplicate['position_ms'] == 0 and sync['position_ms'] >= 125
    assert duplicate['metadata'] == sync['metadata'] == changing['metadata']
    assert all(value['state'] == 'loading' and value['rate'] == 0 for value in values[2:5])
    assert playing['state'] == confirmed['state'] == 'playing' and playing['rate'] == confirmed['rate'] == 1.5
    assert confirmed['metadata']['context_uri'] == 'spotify:album:' + 'c' * 22
    process.stdin.close()
    assert process.wait(timeout=5) == 0

    process = launch('ws-options-fixture')
    assert receive(process)['event'] == 'started'
    values = []
    while True:
        event = receive(process)
        if event.get('clock'):
            values.append(event['clock'])
        if event == dict(event='auth', logged_in=False):
            break
    assert len(values) == 7
    assert values[0]['volume'] == .65 and values[0]['actions']['pause'] and not values[0]['actions']['play']
    assert values[0]['actions']['seek_forward_ms'] == 15000 and values[0]['actions']['seek_backward_ms'] == 30000
    assert values[1]['volume'] == .37
    assert values[2]['options'] == dict(shuffle=True, repeat='all', enhancement='recommendations') and values[2]['actions'] is None
    assert values[4]['actions']['play'] and values[4]['actions']['queue_add'] and values[4]['state'] == 'paused'
    assert values[5]['options'] is None and values[5]['actions'] is None
    assert values[6]['options'] is None
    process.stdin.close()
    assert process.wait(timeout=5) == 0

    for name in ('ws-heartbeat-fixture', 'ws-reconnect-fixture'):
        process = launch(name)
        assert receive(process)['event'] == 'started'
        first = until(process, lambda v: bool(v.get('clock')))['clock']
        if name == 'ws-reconnect-fixture':
            until(process, lambda v: v.get('event') == 'local_control_unavailable')
        second = until(process, lambda v: bool(v.get('clock')) and (name != 'ws-heartbeat-fixture' or v['clock']['uptime'] - first['uptime'] >= .1))['clock']
        assert second['sequence'] > first['sequence'], 'Local reconnect/heartbeat did not produce a fresh clock'
        if name == 'ws-heartbeat-fixture':
            assert .1 <= second['uptime'] - first['uptime'] < .8, 'Owned audio lease needs a heartbeat inside its 800 ms deadline'
        process.stdin.close()
        assert process.wait(timeout=5) == 0

    process = launch('ws-control-fixture')
    assert receive(process)['control_protocol'] == 1
    clock = until(process, lambda event: bool(event.get('clock')))['clock']

    def submit(id, clock, kind, record=None, record_events=None, expires_at=None, required_queue_epoch=None, **fields):
        command = dict(id=id, sequence=clock['sequence'], media_id=clock['media_id'],
                       expires_at_uptime_ms=expires_at if expires_at is not None else int(clock['uptime'] * 1000) + 1500,
                       control=dict(kind=kind, **fields))
        if required_queue_epoch is not None:
            command['required_queue_epoch'] = required_queue_epoch
        line = json.dumps(command).encode() + b'\n'
        assert len(line) <= 512
        process.stdin.write(line)
        def result(event):
            if record_events is not None:
                record_events.append(event)
            if record is not None and event.get('clock'):
                record.append(event['clock'])
            return event.get('event') == 'control_result' and event.get('id') == id
        return until(process, result)

    wrong = dict(clock, media_id='spotify:track:' + 'z' * 22)
    assert not submit(1, wrong, 'pause')['accepted']
    assert submit(2, clock, 'pause')['accepted']
    clock = until(process, lambda event: (event.get('clock') or {}).get('state') == 'paused')['clock']
    assert submit(3, clock, 'seek', position_ms=4321)['accepted']
    clock = until(process, lambda event: (event.get('clock') or {}).get('position_ms') == 4321)['clock']
    assert submit(4, clock, 'volume', percent=72)['accepted']
    clock = until(process, lambda event: (event.get('clock') or {}).get('volume') == .72)['clock']
    repeat_frames = []
    assert submit(5, clock, 'repeat', record=repeat_frames, mode='one')['accepted']
    assert repeat_frames and all(frame['options']['repeat'] == 'all' for frame in repeat_frames), 'Intermediate repeat mode escaped to the host'
    clock = until(process, lambda event: (event.get('clock') or {}).get('options', {}).get('repeat') == 'one')['clock']
    assert submit(6, clock, 'enqueue', uri='spotify:track:' + 'b' * 22)['accepted']
    clock = until(process, lambda event: bool(event.get('clock')))['clock']
    assert submit(7, clock, 'shuffle', enabled=True)['accepted']
    clock = until(process, lambda event: (event.get('clock') or {}).get('options', {}).get('shuffle'))['clock']
    assert submit(8, clock, 'play')['accepted']
    clock = until(process, lambda event: (event.get('clock') or {}).get('state') == 'playing')['clock']
    assert not submit(9, clock, 'start_uri', uri='spotify:artist:' + 'd' * 22)['accepted']
    assert submit(10, clock, 'start_uri', uri='spotify:album:' + 'd' * 22)['accepted'], 'Benign auth and an unrelated error must not preempt a matching URI start ACK'
    clock = until(process, lambda event: (event.get('clock') or {}).get('position_ms', 4321) < 1000)['clock']
    assert not submit(11, clock, 'next')['accepted'], 'Missing ACK must complete as uncertain, without retry'
    until(process, lambda event: event.get('event') == 'local_control_unavailable')
    process.stdin.close()
    assert process.wait(timeout=5) == 0

    for mode in ('shuffle', 'repeat', 'denied', 'media', 'device', 'auth', 'expires'):
        process = launch('ws-option-control-' + mode)
        assert receive(process)['control_protocol'] == 1
        original = until(process, lambda event: bool((event.get('clock') or {}).get('actions')))['clock']
        invalidated = until(process, lambda event: bool(event.get('clock')) and event['clock']['actions'] is None)['clock']
        assert invalidated['media_id'] == original['media_id'] and invalidated['active']
        expires_at = int(invalidated['uptime'] * 1000) + 200 if mode == 'expires' else None
        frames = []
        kind = 'shuffle' if mode == 'shuffle' else 'repeat'
        fields = dict(enabled=True) if kind == 'shuffle' else dict(mode='one')
        result = submit(1, original, kind, record=frames, expires_at=expires_at, **fields)
        assert result['accepted'] == (mode in ('shuffle', 'repeat')), f'Unexpected result after options refresh: {mode}'
        if result['accepted']:
            assert any((frame['actions'] or {}).get(kind) for frame in frames), 'Options accepted without renewed actions'
        process.stdin.close()
        assert process.wait(timeout=5) == 0 and process.stderr.read(4096) == b''

    process = launch('ws-relative-skip-fixture')
    assert receive(process)['control_protocol'] == 1
    original = until(process, lambda event: bool(event.get('clock')))['clock']
    changing = until(process, lambda event: (event.get('clock') or {}).get('state') == 'loading')['clock']
    assert original['actions']['next'] and changing['actions'] is None
    frames = []
    assert submit(1, original, 'next', record=frames)['accepted'], 'Next must survive an owned track handoff'
    assert any(frame['media_id'] == changing['media_id'] and (frame['actions'] or {}).get('next') for frame in frames)
    assert not submit(2, original, 'seek', position_ms=4321)['accepted'], 'Seek must retain the original item fence'
    assert not submit(3, original, 'pause', required_queue_epoch=original['queue_epoch'])['accepted'], 'Strict Pause must not hand off to another track'
    assert submit(4, original, 'pause')['accepted'], 'Ordinary Pause must address the same owned output after the track changes'
    process.stdin.close()
    assert process.wait(timeout=5) == 0

    process = launch('ws-required-next-fixture')
    assert receive(process)['control_protocol'] == 1
    original = until(process, lambda event: bool((event.get('clock') or {}).get('actions')))['clock']
    invalidated = until(process, lambda event: bool(event.get('clock')) and event['clock']['actions'] is None)['clock']
    assert invalidated['media_id'] == original['media_id'] and invalidated['queue_epoch'] == original['queue_epoch']
    frames = []
    assert not submit(1, original, 'next', record=frames, required_queue_epoch=original['queue_epoch'])['accepted'], 'Scoped Next must stop waiting for actions when the occurrence changes'
    successor = next((frame for frame in frames if frame['media_id'] != original['media_id'] and (frame['actions'] or {}).get('next')), None)
    if successor is None:
        successor = until(process, lambda event: bool(clock := event.get('clock')) and clock['media_id'] != original['media_id'] and (clock['actions'] or {}).get('next'))['clock']
    assert successor['queue_epoch'] > original['queue_epoch']
    assert not submit(2, successor, 'next', required_queue_epoch=0)['accepted']
    assert not submit(3, successor, 'next', required_queue_epoch=original['queue_epoch'])['accepted'], 'A fresh clock cannot authorize Next with a stale occurrence epoch'
    assert submit(4, successor, 'next', required_queue_epoch=successor['queue_epoch'])['accepted'], 'Next must reach the matched occurrence'
    process.stdin.close()
    assert process.wait(timeout=5) == 0 and process.stderr.read(4096) == b''

    process = launch('ws-required-play-fixture')
    assert receive(process)['control_protocol'] == 1
    original = until(process, lambda event: bool((event.get('clock') or {}).get('actions')))['clock']
    epoch = original['queue_epoch']
    assert original['state'] == 'paused' and original['position_ms'] == 0
    assert not submit(1, original, 'play', required_queue_epoch=0)['accepted']
    assert not submit(2, original, 'play', required_queue_epoch=epoch + 1)['accepted']
    assert not submit(3, dict(original, media_id='spotify:track:' + 'b' * 22), 'play',
                      required_queue_epoch=epoch)['accepted']
    assert not submit(4, original, 'play', required_queue_epoch=epoch, expires_at=uptime_ms() - 1)['accepted']
    assert submit(5, original, 'volume', percent=47)['accepted']
    denied = until(process, lambda event: (event.get('clock') or {}).get('volume') == .47)['clock']
    assert not submit(6, denied, 'play', required_queue_epoch=epoch)['accepted'], 'The full clock denied Play'
    assert submit(7, denied, 'volume', percent=48)['accepted']
    advanced = until(process, lambda event: (event.get('clock') or {}).get('volume') == .48)['clock']
    assert advanced['position_ms'] == 121
    assert not submit(8, advanced, 'play', required_queue_epoch=epoch)['accepted'], 'Paused outside the start interval'
    assert submit(9, advanced, 'volume', percent=52)['accepted']
    playing = until(process, lambda event: (event.get('clock') or {}).get('state') == 'playing')['clock']
    current = until(process, lambda event: (event.get('clock') or {}).get('state') == 'paused')['clock']
    assert current['queue_epoch'] == playing['queue_epoch'] == epoch and current['sequence'] > playing['sequence']
    assert not submit(10, original, 'play', required_queue_epoch=epoch)['accepted'], 'A new pause revived an old Play proof'
    assert submit(11, current, 'play', required_queue_epoch=epoch)['accepted'], 'The exact paused occurrence did not receive Play'
    current = until(process, lambda event: (event.get('clock') or {}).get('state') == 'playing')['clock']
    assert not submit(12, current, 'play', required_queue_epoch=epoch)['accepted'], 'Scoped Play escaped its paused state'
    process.stdin.close()
    assert process.wait(timeout=5) == 0 and process.stderr.read(4096) == b''

    confirmed_next = ('early-ack', 'late-ack', 'repeat-context-rotation', 'repeat-context-delayed')
    rejected_next = ('unchanged', 'reorder', 'no-ack', 'no-buffering', 'repeat-context-conflicting-uid',
                     'repeat-context-retained-head', 'repeat-context-repeated-head', 'repeat-context-reshuffled-tail')
    unrelated_head = 'repeat-context-unrelated-head'
    for mode in confirmed_next + rejected_next + (unrelated_head,):
        process = launch('ws-duplicate-next-' + mode)
        assert receive(process)['control_protocol'] == 1
        initial = until(process, lambda event: bool((event.get('clock') or {}).get('actions')))['clock']
        queue = until(process, lambda event: event.get('event') == 'queue')['queue']
        assert queue['epoch'] == initial['queue_epoch'] and queue['media_id'] == initial['media_id']
        context_repeat = mode.startswith('repeat-context-')
        assert initial['options']['repeat'] == ('all' if context_repeat else 'off')
        assert len(queue['upcoming']) == (10 if context_repeat else 4)
        assert (queue['upcoming'][0]['media_id'] == initial['media_id']) == (mode != unrelated_head)
        if context_repeat:
            head_source = 'context' if mode == 'repeat-context-repeated-head' else 'queue'
            assert [row['source'] for row in queue['upcoming']] == [head_source] + ['context'] * 9
            assert [row['media_id'] for row in queue['upcoming'][1:6]] == [
                'spotify:track:' + media * 22 for media in 'bdefa'
            ]
            assert queue['upcoming'][6:] == queue['upcoming'][1:5]
        dispatch_clock = initial
        if mode in ('repeat-context-delayed', unrelated_head):
            # The fixture only returns the idle queue on get_queue. Wait past
            # the initial evidence's 1.2 s expiry without any queue mutations.
            refreshed = []

            def after_original_queue_expires(event):
                if event.get('event') == 'queue':
                    refreshed.append(event['queue'])
                clock = event.get('clock')
                return bool(clock and clock['uptime'] - queue['uptime'] >= 1.5)

            dispatch_clock = until(process, after_original_queue_expires)['clock']
            if mode == unrelated_head:
                assert not refreshed, 'An unrelated queue head was polled on the duplicate cadence'
                process.stdin.close()
                assert process.wait(timeout=5) == 0 and process.stderr.read(4096) == b''
                continue
            assert refreshed and dispatch_clock['uptime'] - refreshed[-1]['uptime'] <= 1.2, 'Duplicate queue proof expired at idle'
            assert refreshed[-1]['upcoming'] == queue['upcoming'] and refreshed[-1]['epoch'] == queue['epoch']
        frames = []
        wire_id = 37
        result = submit(wire_id, dispatch_clock, 'next', record=frames)
        assert result['accepted'] == (mode != 'no-ack')
        marker = next((frame for frame in frames if frame['volume'] == .62), None)
        if marker is None:
            def completed(event):
                if not (clock := event.get('clock')):
                    return False
                frames.append(clock)
                return clock['volume'] == .62
            marker = until(process, completed)['clock']
        confirmed = mode in confirmed_next
        epoch = initial['queue_epoch'] + confirmed
        assert marker['queue_epoch'] == epoch, f'Unexpected same-URI transition epoch: {mode}'
        assert marker['media_id'] == initial['media_id'] and marker['state'] == 'playing' and marker['rate'] > 0
        assert marker['sequence'] > initial['sequence'] and marker['actions']['next']
        assert all(frame['queue_epoch'] == initial['queue_epoch'] for frame in frames if frame['state'] == 'loading')
        confirmations = [frame for frame in frames if 'confirmed_same_media_next' in frame]
        assert [frame['confirmed_same_media_next'] for frame in confirmations] == ([wire_id] if confirmed else []), mode
        assert all(frame['queue_epoch'] == epoch and frame['media_id'] == initial['media_id']
                   and frame['state'] == 'playing' and frame['rate'] > 0 for frame in confirmations)
        if mode == 'repeat-context-rotation':
            assert not submit(38, marker, 'pause', required_queue_epoch=initial['queue_epoch'])['accepted'], 'A fresh clock on the same URI must not admit a Pause for the old occurrence'
            assert not submit(39, marker, 'pause', required_queue_epoch=0)['accepted']
            assert not submit(40, marker, 'seek', position_ms=100, required_queue_epoch=epoch)['accepted']
            assert submit(41, marker, 'pause', required_queue_epoch=epoch)['accepted'], 'Strict Pause should reach the matched successor occurrence'
        process.stdin.close()
        assert process.wait(timeout=5) == 0 and process.stderr.read(4096) == b''

    buffering_successor_queue = ('buffering-ack-first', 'buffering-state-first')
    for mode in ('proven', 'unproven', 'invalidated', 'ordinary', 'future-play', 'future-uri',
                 'missing', 'wrong-track', 'same-track-restart'):
        process = launch('ws-play-ack-loss-' + mode)
        assert receive(process)['control_protocol'] == 1
        paused = until(process, lambda event: (event.get('clock') or {}).get('state') == 'paused')['clock']
        events = []

        def recorded_clock(state):
            def match(event):
                events.append(event)
                return (event.get('clock') or {}).get('state') == state
            return next((event['clock'] for event in reversed(events)
                         if (event.get('clock') or {}).get('state') == state), None) or until(process, match)['clock']

        def assert_wire_healthy():
            assert all(event['event'] not in ('auth', 'local_control_unavailable')
                       and (event['event'] != 'clock' or event.get('clock') is not None)
                       for event in events), 'The active clock and authentication were interrupted'

        def assert_wire_invalidated():
            if not any(event['event'] == 'local_control_unavailable' for event in events):
                until(process, lambda event: event['event'] == 'local_control_unavailable')
            if not any(event['event'] == 'clock' and event.get('clock') is None for event in events):
                assert until(process, lambda event: event['event'] == 'clock')['clock'] is None

        started = time.monotonic()
        proven = mode not in ('unproven', 'invalidated')
        assert submit(1, paused, 'play', record_events=events)['accepted'] == proven, mode
        elapsed = time.monotonic() - started
        if not proven:
            assert elapsed >= 1.05, f'Unproven Play retired before the ordinary timeout: {elapsed:.3f}s ({mode})'
            assert_wire_invalidated()
        else:
            if mode != 'ordinary':
                assert .10 <= elapsed < .60, f'Expected the short observed-Play grace, took {elapsed:.3f}s ({mode})'
            else:
                time.sleep(.16)  # A prompt ACK must cancel the outstanding grace timer.
            playing = recorded_clock('playing')
            pause_started = time.monotonic()
            assert submit(2, playing, 'pause', record_events=events, required_queue_epoch=paused['queue_epoch'])['accepted']
            if mode == 'proven':
                assert time.monotonic() - pause_started >= .035, 'A late Play ACK was mistaken for the Pause ACK'
            successor = recorded_clock('paused')
            assert successor['queue_epoch'] == paused['queue_epoch'], 'Pause must use the still-open active occurrence'
            assert_wire_healthy()
            if mode != 'ordinary':
                mode_control = {'kind': 'play'}
                if mode == 'future-uri':
                    mode_control = {'kind': 'start_uri', 'uri': 'spotify:album:' + 'd' * 22}
                command = submit(3, successor, record_events=events, **mode_control)
                if mode in ('future-play', 'future-uri'):
                    assert time.monotonic() - started >= .42, 'New play write completed before its earlier ACK could drain'
                if mode in ('missing', 'wrong-track', 'same-track-restart'):
                    assert time.monotonic() - started >= 1.05, 'Colliding command replaced the wire before the ordinary timeout'
                if mode in ('wrong-track', 'same-track-restart'):
                    assert not command['accepted'], 'The replacement no longer owns the same occurrence'
                    assert_wire_invalidated()
                else:
                    assert command['accepted'], f'Later Play or URI start did not follow its own ACK ({mode})'
                    assert_wire_healthy()
        process.stdin.close()
        assert process.wait(timeout=5) == 0 and process.stderr.read(4096) == b''

    previous_successor_queue = ('queue-ack-first', 'queue-state-first') + buffering_successor_queue
    confirmed_previous = ('paused', 'playing', 'early-cue') + previous_successor_queue
    for mode in confirmed_previous + ('stale-cue', 'no-action', 'wrong-context', 'no-seek-ack', 'no-skip-ack'):
        process = launch('ws-previous-from-start-' + mode)
        assert receive(process)['control_protocol'] == 1
        clock = until(process, lambda event: (event.get('clock') or {}).get('actions'))['clock']
        epoch = clock['queue_epoch']
        if mode in previous_successor_queue:
            until(process, lambda event: (event.get('queue') or {}).get('epoch') == epoch)
        assert not submit(1, clock, 'previous_from_start')['accepted'], 'Compound Previous must bind its outgoing occurrence'
        assert not submit(2, clock, 'previous_from_start', required_queue_epoch=epoch + 1)['accepted']
        events = []
        assert submit(3, clock, 'previous_from_start', required_queue_epoch=epoch, record_events=events)['accepted'] == (mode in confirmed_previous), mode
        if mode in previous_successor_queue:
            def successor_queue(event):
                return (event.get('queue') or {}).get('media_id') == 'spotify:track:' + 'b' * 22
            def record_successor_queue(event):
                events.append(event)
                return successor_queue(event)
            successor = next((event for event in events if successor_queue(event)), None) or until(process, record_successor_queue)
            successor = successor['queue']
            assert successor['epoch'] > epoch
            assert [row['media_id'] for row in successor['upcoming']] == [clock['media_id']], 'A queue pushed before the full successor clock was relabeled'
            if mode in buffering_successor_queue:
                assert any((frame := event.get('clock') or {}).get('media_id') == successor['media_id']
                           and frame.get('state') == 'loading' and frame.get('rate') == 0
                           and (frame.get('actions') or {}).get('next') is True for event in events), 'Missing actionable Buffering clock'
        process.stdin.close()
        assert process.wait(timeout=5) == 0 and process.stderr.read(4096) == b''

    completed_relative = ('complete', 'stale-complete', 'itemless-playing', 'missing-uri-playing', 'malformed-playing', 'empty-uri-slow')
    for mode, control in [('complete', 'next'), ('complete', 'previous'), ('stale-complete', 'next'), ('inactive', 'next'), ('expires', 'next'),
                          ('itemless-playing', 'next'), ('missing-uri-playing', 'next'), ('malformed-playing', 'next'),
                          ('invalid-position-playing', 'next'), ('empty-uri-slow', 'next'), ('empty-uri-expires', 'next'),
                          ('empty-uri-changes-shape', 'next'), ('malformed-expires', 'next')]:
        process = launch('ws-incomplete-relative-' + mode)
        assert receive(process)['control_protocol'] == 1
        original = until(process, lambda event: bool(event.get('clock')))['clock']
        frames = []
        events = []
        started_at = time.monotonic()
        result = submit(1, original, control, record=frames, record_events=events)
        assert result['accepted'] == (mode != 'inactive')
        cleared = {'event': 'clock', 'clock': None}
        if mode in ('inactive', 'invalid-position-playing'):
            assert not any(frame['state'] == 'loading' for frame in frames)
            assert cleared in events or until(process, lambda event: event == cleared)['clock'] is None
        else:
            assert cleared not in events
            incomplete = next((frame for frame in frames if frame['state'] == 'loading'), None)
            if incomplete is None:
                transitional = until(process, lambda event: event == cleared or (event.get('clock') or {}).get('state') == 'loading')
                assert transitional != cleared, 'An active incomplete transition must not clear the clock'
                incomplete = transitional['clock']
            assert incomplete['media_id'] == original['media_id'] and incomplete['rate'] == 0 and incomplete['active']
            assert incomplete['actions'] is None and incomplete['metadata'] is None and incomplete['options'] is None
            if mode in completed_relative:
                next_event = until(process, lambda event: event == cleared or (event.get('clock') or {}).get('media_id') == 'spotify:track:' + 'b' * 22)
                assert next_event != cleared, 'The clock must survive until the exact successor arrives'
                successor = next_event['clock']
                assert successor['state'] == 'playing' and successor['actions']['pause']
                if mode == 'empty-uri-slow':
                    assert successor['uptime'] - original['uptime'] > 2
                assert submit(2, successor, 'pause')['accepted'], 'The queued Pause must be admissible on the complete successor'
                paused = until(process, lambda event: (event.get('clock') or {}).get('state') == 'paused')['clock']
                assert paused['media_id'] != original['media_id']
            else:
                assert until(process, lambda event: event == cleared)['clock'] is None
                elapsed = time.monotonic() - started_at
                if mode == 'empty-uri-expires':
                    assert 3.25 <= elapsed < 4.25, 'Repeated empty URIs must use one fixed extended deadline'
                elif mode == 'empty-uri-changes-shape':
                    assert 2 <= elapsed < 3.25, 'An unrelated malformed item must immediately clear the extended transition'
                elif mode == 'malformed-expires':
                    assert 1.25 <= elapsed < 2.25, 'Other malformed identifiers keep the standard deadline'
        process.stdin.close()
        assert process.wait(timeout=5) == 0

    process = launch('ws-incomplete-relative-off-lane-malformed-playing')
    assert receive(process)['control_protocol'] == 1
    until(process, lambda event: bool(event.get('clock')))
    after = until(process, lambda event: event == cleared or (event.get('clock') or {}).get('state') == 'loading')
    assert after == cleared, 'Malformed Playing without a relative command must clear the clock'
    process.stdin.close()
    assert process.wait(timeout=5) == 0

    for name, logged_in in [('ws-control-logout-fixture', False), ('ws-control-inactive-fixture', True)]:
        process = launch(name)
        assert receive(process)['control_protocol'] == 1
        clock = until(process, lambda event: bool(event.get('clock')))['clock']
        assert not submit(1, clock, 'start_uri', uri='spotify:album:' + 'd' * 22)['accepted'], 'A late ACK must not override lost authentication or ownership'
        assert until(process, lambda event: event.get('event') == 'auth') == dict(event='auth', logged_in=logged_in)
        until(process, lambda event: event.get('event') == 'local_control_unavailable')
        process.stdin.close()
        assert process.wait(timeout=5) == 0

    for mode in ('single', 'album-last', 'album-next', 'unknown', 'seek', 'pause', 'repeat-track', 'repeat-context'):
        process = launch('ws-autoplay-' + mode)
        assert receive(process)['control_protocol'] == 1
        clock = until(process, lambda event: bool(event.get('clock')))['clock']
        queue_event = lambda event: event.get('event') == 'queue'
        first = until(process, queue_event)['queue']
        assert 'autoplay_ready' not in first
        target = 'spotify:track:' + 'b' * 22
        if mode == 'single':
            assert not submit(1, clock, 'autoplay_enqueue', uri=target)['accepted'], 'One empty read is insufficient'
        # Ordinary queue polling waits 12 seconds. A second reply here proves
        # the helper requests confirmation while the current item is near EOF.
        second = until(process, queue_event)['queue']
        assert first['epoch'] == second['epoch'] and .2 <= second['uptime'] - first['uptime'] < 1.2
        assert second.get('autoplay_ready', False) == (mode not in ('album-next', 'unknown'))
        clock = until(process, lambda event: (event.get('clock') or {}).get('uptime', 0) >= second['uptime'])['clock']
        if mode in ('single', 'album-last'):
            if mode == 'album-last':
                assert clock['metadata']['context_uri'] == 'spotify:album:' + 'c' * 22
            assert not submit(2, clock, 'autoplay_enqueue', uri=clock['media_id'])['accepted'], 'Autoplay repeated the seed'
            assert submit(3, clock, 'autoplay_enqueue', uri=target)['accepted']
            clock = until(process, lambda event: bool(event.get('clock')))['clock']
            assert not submit(4, clock, 'autoplay_enqueue', uri=target)['accepted'], 'An ACK without a queue update cannot admit a duplicate'
        elif mode in ('album-next', 'unknown'):
            assert bool(second['upcoming']) == (mode == 'album-next')
            assert not submit(1, clock, 'autoplay_enqueue', uri=target)['accepted'], 'Autoplay displaced a native upcoming entry'
        else:
            if mode == 'pause':
                assert submit(1, clock, 'pause')['accepted']
                matches = lambda value: value.get('state') == 'paused'
            elif mode == 'seek':
                assert submit(1, clock, 'seek', position_ms=20000)['accepted']
                matches = lambda value: 20000 <= value.get('position_ms', 0) < 21000
            else:
                repeat = 'all' if mode == 'repeat-context' else 'one'
                assert submit(1, clock, 'repeat', mode=repeat)['accepted']
                matches = lambda value: (value.get('options') or {}).get('repeat') == repeat
            changed_frames = []
            assert not submit(2, clock, 'autoplay_enqueue', record=changed_frames, uri=target)['accepted'], 'A manual control must fence an already-planned recommendation'
            changed = next((frame for frame in changed_frames if matches(frame)), None) or until(
                process, lambda event: matches(event.get('clock') or {})
            )['clock']
            assert not submit(3, changed, 'autoplay_enqueue', uri=target)['accepted']
        process.stdin.close()
        assert process.wait(timeout=5) == 0 and process.stderr.read(4096) == b''

    process = subprocess.run([helper], input=b"x" * 16385 + b"\n", stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5)
    assert process.returncode == 2 and not process.stdout, "Oversized startup command launched a process"

print("Spotify supervisor passed: pipe lifetime, bounded clocks and queue, URI starts, local controls, autoplay fences, ACK timeout and reconnect.")

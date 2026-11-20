"""Original account-free Soloist wire fixture; launched only by the supervisor test."""
import base64
import hashlib
import json
import os
import pathlib
import select
import socket
import struct
import sys
import time

name = sys.argv[sys.argv.index('--device-name') + 1]
if name == 'volume-zero':
    sys.exit(0 if sys.argv[sys.argv.index('--initial-volume') + 1] == '0' else 7)
if name == 'expired':
    sys.exit(10)
if not name.startswith('ws-'):
    while True:
        time.sleep(.1)

directory = pathlib.Path(sys.argv[sys.argv.index('--data-dir') + 1])
directory.mkdir(exist_ok=True)
server = socket.socket()
server.bind(('127.0.0.1', 0))
server.listen()
server.settimeout(10)
(directory / 'soloist.pid').write_text(str(os.getpid()))
(directory / 'ws.addr').write_text('127.0.0.1')
(directory / 'ws.port').write_text(str(server.getsockname()[1]))


def handshake():
    connection, _ = server.accept()
    connection.settimeout(10)
    reader = connection.makefile('rb', buffering=0)
    headers = {}
    while True:
        line = reader.readline(4096).decode().strip()
        if not line:
            break
        if ':' in line:
            key, value = line.split(':', 1)
            headers[key.lower()] = value.strip()
    accept = base64.b64encode(hashlib.sha1((headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
    connection.sendall(('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + '\r\n\r\n').encode())
    return connection, reader


def send(connection, value, opcode=1):
    data = json.dumps(value).encode() if isinstance(value, dict) else value
    header = bytes([128 | opcode, len(data)]) if len(data) < 126 else bytes([128 | opcode, 126]) + struct.pack('!H', len(data))
    connection.sendall(header + data)


def auth(connection, value):
    send(connection, dict(type='auth_state', logged_in=value, is_active=value))


def anchor(position=1000):
    return dict(position_ms=position, timestamp_ms=int(time.time() * 1000), speed=1.5)


def playback(connection, **changes):
    value = dict(type='playback_state', status='playing', is_active=True,
                         item=dict(uri='spotify:track:' + 'a' * 22, decorations=dict(identity=dict(name='fixture-key'))),
                         position=anchor(), private_fixture_field='fixture-key')
    value.update(changes)
    send(connection, value)


def read_frame(reader):
    def exact(count):
        value = b''
        while len(value) < count:
            chunk = reader.read(count - len(value))
            if not chunk:
                sys.exit(0)
            value += chunk
        return value

    header = exact(2)
    opcode, count = header[0] & 15, header[1] & 127
    if count == 126:
        count = struct.unpack('!H', exact(2))[0]
    assert count < 4096
    mask = exact(4) if header[1] & 128 else b'\0' * 4
    return opcode, bytes(byte ^ mask[i % 4] for i, byte in enumerate(exact(count)))


connection, reader = handshake()
auth(connection, True)
if name.startswith('ws-bootstrap-'):
    mode = name.removeprefix('ws-bootstrap-')
    active = mode == 'credit'
    logged_in = mode != 'unauthenticated'
    if not logged_in:
        auth(connection, False)

    def bootstrap_state():
        if logged_in:
            position = anchor(1000 if active else 0)
            position['speed'] = 1 if active else 0
            playback(connection, status='playing' if active else 'idle', is_active=active,
                     item=dict(uri='spotify:track:' + 'a' * 22) if active else dict(uri=''),
                     position=position, options=dict(shuffle=False, repeat='off'),
                     available_actions=dict(pause={}, play={}) if active else {})

    bootstrap_state()
    activations = 0
    while True:
        opcode, raw = read_frame(reader)
        if opcode == 8:
            break
        if opcode == 9:
            send(connection, raw, opcode=10)
            continue
        request = json.loads(raw)
        command = request['command']
        if command == 'get_state':
            if mode != 'stale':
                bootstrap_state()
        elif command == 'activate':
            assert mode not in ('forbidden', 'stale', 'unauthenticated'), 'An ineligible bootstrap reached the SDK'
            activations += 1
            assert activations == 1, 'Bootstrap was retried on the same socket'
            send(connection, dict(type='volume_changed', volume=23))
            if mode == 'wrong-ack':
                send(connection, dict(type='command_result', command='play'))
                time.sleep(.15)
            if mode == 'auth-loss':
                logged_in = False
                auth(connection, False)
            elif mode not in ('withheld', 'credit'):
                send(connection, dict(type='command_result', command='activate'))
        else:
            raise AssertionError(f'Unexpected bootstrap command: {command}')
elif name == 'ws-fixture':
    send(connection, dict(type='playback_state', private_fixture_field='fixture-key'))
    time.sleep(.15)
    auth(connection, False)
elif name == 'ws-clock-fixture':
    playback(connection)
    time.sleep(.05)
    send(connection, dict(type='position_sync', position=anchor(4321)))
    time.sleep(.05)
    send(connection, dict(type='playback_changed', status='paused'))
    time.sleep(.05)
    send(connection, dict(type='device_changed', is_active=False, device_name='fixture-key'))
    time.sleep(.05)
    send(connection, dict(type='position_sync', position=dict(position_ms=True, timestamp_ms=0, speed=9)))
    auth(connection, False)
elif name == 'ws-options-fixture':
    actions = dict(pause={}, seek={}, skip_next={}, add_to_queue={}, seek_forward=dict(step_ms=15000), seek_backward=dict(step_ms=30000))
    playback(connection, volume=65, options=dict(shuffle=False, repeat='off', modes=dict(context_enhancement='NONE')), available_actions=actions)
    time.sleep(.05)
    send(connection, dict(type='volume_changed', volume=37))
    time.sleep(.05)
    options = dict(shuffle=True, repeat='context', modes=dict(context_enhancement='RECOMMENDATION', private_fixture_field='fixture-key'))
    send(connection, dict(type='options_changed', options=options))
    time.sleep(.05)
    send(connection, dict(type='playback_changed', status='paused'))
    time.sleep(.05)
    playback(connection, status='paused', volume=37, options=options, available_actions=dict(play={}, seek={}, add_to_queue={}, seek_backward=dict(step_ms=30000)))
    time.sleep(.05)
    send(connection, dict(type='context_changed', context=dict(uri='spotify:album:' + 'b' * 22, private_fixture_field='fixture-key')))
    time.sleep(.05)
    send(connection, dict(type='options_changed', options=dict(shuffle=1, repeat='off')))
    send(connection, dict(type='volume_changed', volume=True))
    auth(connection, False)
elif name == 'ws-metadata-fixture':
    album = dict(entity=dict(decorations=dict(identity=dict(name='Original album'), visual_identity=dict(cover=[
        dict(url='https://i.scdn.co/image/fallback', size='small'),
        dict(url='https://i.scdn.co/image/large', size='large'),
        dict(url='https://i.scdn.co/image/large?secret=fixture-key', size='xlarge'),
    ]))))
    creators = [dict(entity=dict(decorations=dict(identity=dict(name='Original artist'))))]
    item = dict(uri='spotify:track:' + 'a' * 22, decorations=dict(
        identity=dict(name='Original title'), playback=dict(duration_ms=65432), parent=album, creators=creators,
        private_fixture_field='fixture-key',
    ))
    playback(connection, item=item, context=dict(uri='spotify:album:' + 'b' * 22))
    send(connection, dict(type='context_changed', context=dict(uri='spotify:playlist:' + 'c' * 22)))
    next_item = dict(uri='spotify:track:' + 'd' * 22)
    send(connection, dict(type='track_changed', item=next_item))
    playback(connection, item=dict(next_item, decorations=dict(identity=dict(name='Invalid duration'), playback=dict(duration_ms=True))))
    auth(connection, False)
elif name == 'ws-track-transition-fixture':
    def decorated(id, title):
        return dict(uri='spotify:track:' + id * 22, decorations=dict(
            identity=dict(name=title), playback=dict(duration_ms=65000),
            visual_identity=dict(cover=[dict(url='https://i.scdn.co/image/' + id, size='large')]),
        ))

    first = decorated('a', 'Original current track')
    next_item = decorated('b', 'Original next track')
    context = dict(uri='spotify:album:' + 'c' * 22)
    playback(connection, item=first, context=context, position=anchor(4300))
    opcode, request = read_frame(reader)
    assert opcode == 1 and json.loads(request) == dict(type='command', command='get_state')
    send(connection, dict(type='track_changed', item=decorated('a', 'Updated current track')))
    send(connection, dict(type='track_changed', item=next_item))
    time.sleep(.15)
    playback(connection, item=first, context=context, position=anchor(4400))  # Pending get_state reply.
    send(connection, dict(type='track_changed', item=dict(uri=next_item['uri'])))
    send(connection, dict(type='position_sync', position=anchor(125)))
    send(connection, dict(type='playback_changed', status='playing'))
    time.sleep(.15)
    playback(connection, item=next_item, context=context, position=anchor(520))
    auth(connection, False)
elif name.startswith('ws-play-ack-loss-'):
    mode = name.removeprefix('ws-play-ack-loss-')
    context = dict(uri='spotify:album:' + 'c' * 22)
    status = 'paused'
    delayed = mode in ('future-play', 'future-uri')
    replace_wire = mode in ('missing', 'wrong-track', 'same-track-restart')

    def read_play_commands(connected, stream, recovered=False):
        global status, context
        first_play = not recovered
        play_ack_at = None
        item = 'b' if recovered and mode == 'wrong-track' else 'a'
        restarted = recovered and mode == 'same-track-restart'

        def play_state():
            clock = anchor(0 if restarted else 3200)
            clock['speed'] = int(status == 'playing')
            playback(connected, status=status, position=clock, context=context,
                     item=dict(uri='spotify:track:' + item * 22),
                     available_actions={('play' if status == 'paused' else 'pause'): {}})

        play_state()
        while True:
            if play_ack_at is not None:
                delay = max(0, play_ack_at - time.monotonic())
                if not select.select([connected], [], [], delay)[0]:
                    send(connected, dict(type='command_result', command='play'))
                    play_ack_at = None
            opcode, data = read_frame(stream)
            if opcode == 8:
                return
            if opcode == 9:
                send(connected, data, opcode=10)
                continue
            request = json.loads(data)
            command = request['command']
            if command == 'get_state':
                play_state()
            elif command == 'get_queue':
                send(connected, dict(type='queue_changed', previous=[], upcoming=[]))
            elif command == 'play':
                assert not replace_wire or first_play or recovered, 'Undrained Play leaked onto its original wire'
                assert play_ack_at is None, 'New Play reached the wire before its earlier ACK drained'
                if 'uri' in request:
                    assert request['uri'] == 'spotify:album:' + 'd' * 22
                    context = dict(uri=request['uri'])
                if not first_play or mode != 'unproven':
                    status = 'playing'
                    play_state()
                if first_play and mode == 'invalidated':
                    time.sleep(.035)
                    status = 'paused'
                    play_state()
                if first_play and delayed:
                    play_ack_at = time.monotonic() + .45
                if not first_play or mode == 'ordinary':
                    time.sleep(.035)
                    send(connected, dict(type='command_result', command=command))
                first_play = False
            elif command == 'pause':
                assert not recovered and status == 'playing', 'Pause must reach the original wire'
                if mode == 'proven':
                    send(connected, dict(type='command_result', command='play'))
                    time.sleep(.045)
                status = 'paused'
                play_state()
                send(connected, dict(type='command_result', command=command))
            else:
                raise AssertionError(f'Unexpected command: {command}')

    read_play_commands(connection, reader)
    if replace_wire or mode in ('unproven', 'invalidated'):
        reader.close()
        connection.close()
        connection, reader = handshake()
        auth(connection, True)
        read_play_commands(connection, reader, recovered=True)
elif name in ('ws-relative-skip-fixture', 'ws-required-next-fixture'):
    actions = dict(seek={}, skip_next={}, skip_prev={}, pause={})
    playback(connection, available_actions=actions)
    opcode, request = read_frame(reader)
    assert opcode == 1 and json.loads(request)['command'] == 'get_state'
    next_item = dict(uri='spotify:track:' + 'b' * 22)
    if name == 'ws-required-next-fixture':
        send(connection, dict(type='options_changed', options=dict(shuffle=False, repeat='off')))
    else:
        send(connection, dict(type='track_changed', item=next_item))
    release_at = time.monotonic() + .25
    while time.monotonic() < release_at:
        if not select.select([connection], [], [], max(0, release_at - time.monotonic()))[0]:
            break
        opcode, request = read_frame(reader)
        assert opcode == 1 and json.loads(request)['command'] in ('get_state', 'get_queue'), 'Control dispatched before fresh actions'
    if name == 'ws-required-next-fixture':
        send(connection, dict(type='track_changed', item=next_item))
    playback(connection, item=next_item, available_actions=actions)
    sent = False
    while True:
        opcode, request = read_frame(reader)
        if opcode == 8:
            break
        assert opcode == 1
        command = json.loads(request)['command']
        if command == 'skip_next':
            assert not sent, 'Relative skip dispatched more than once'
            sent = True
            send(connection, dict(type='command_result', command=command))
        elif command == 'pause':
            send(connection, dict(type='command_result', command=command))
        elif command == 'get_state':
            playback(connection, item=next_item, available_actions=actions)
        elif command == 'get_queue':
            send(connection, dict(type='queue_changed', previous=[], upcoming=[]))
        else:
            raise AssertionError(f'Unexpected command: {command}')
elif name == 'ws-required-play-fixture':
    context = dict(uri='spotify:album:' + 'c' * 22)
    state, position, volume = 'paused', 0, 41

    def scoped_play_state():
        clock = anchor(position)
        clock['speed'] = int(state == 'playing')
        actions = dict(pause={}, play={}) if volume != 47 else dict(pause={})
        playback(connection, status=state, position=clock, context=context, volume=volume,
                 available_actions=actions)

    scoped_play_state()
    played = False
    while True:
        opcode, raw = read_frame(reader)
        if opcode == 8:
            break
        if opcode == 9:
            send(connection, raw, opcode=10)
            continue
        request = json.loads(raw)
        command = request['command']
        if command == 'get_state':
            scoped_play_state()
        elif command == 'get_queue':
            send(connection, dict(type='queue_changed', previous=[], upcoming=[]))
        elif command == 'set_volume':
            volume = request['volume']
            send(connection, dict(type='command_result', command=command))
            if volume == 52:
                # Spotify can play and pause externally while keeping this URI
                # and queue occurrence. An older scoped Play must be rejected.
                state = 'playing'
                scoped_play_state()
                state = 'paused'
            position = 121 if volume == 48 else 0
            scoped_play_state()
        elif command == 'play':
            assert volume == 52 and state == 'paused' and not played, 'Play escaped its exact pause or was sent twice'
            played = True
            send(connection, dict(type='command_result', command=command))
            state = 'playing'
            scoped_play_state()
        else:
            raise AssertionError(f'Unexpected command: {command}')
elif name.startswith('ws-duplicate-next-'):
    mode = name.removeprefix('ws-duplicate-next-')
    actions = dict(skip_next={}, skip_prev={}, seek={}, pause={})
    context = dict(uri='spotify:album:' + 'c' * 22)

    def occurrence(uid, media, source='context'):
        return dict(uid='fixture-key-' + str(uid), source=source,
                    item=dict(uri='spotify:track:' + media * 22))

    context_repeat = mode.startswith('repeat-context-')
    previous = [occurrence('previous', 'f')]
    if context_repeat:
        queued_media = 'g' if mode == 'repeat-context-unrelated-head' else 'a'
        queued = occurrence('queued', queued_media, source='queue')
        cycle = [occurrence(uid, media) for uid, media in enumerate('bdefa', start=1)]
        # A short album repeats its context UIDs in Soloist's capped queue.
        original = [queued, *cycle, *cycle[:4]]
        successor = cycle * 2
        if mode == 'repeat-context-conflicting-uid':
            successor[-1] = occurrence(5, 'h')
        elif mode == 'repeat-context-retained-head':
            successor[-1] = queued
        elif mode == 'repeat-context-repeated-head':
            original[0] = cycle[-1]
        elif mode == 'repeat-context-reshuffled-tail':
            successor[6], successor[7] = successor[7], successor[6]
    else:
        original = [occurrence(uid, media) for uid, media in enumerate('abde')]
        successor = {'unchanged': original, 'reorder': original[2:3] + original[1:2] + original[3:]}.get(mode, original[1:])
    upcoming = original
    moved = False

    def duplicate_state(status='playing', position=0, volume=53):
        clock = anchor(position)
        clock['speed'] = 0 if status == 'buffering' else 1
        playback(connection, status=status, context=context, position=clock, volume=volume,
                 options=dict(shuffle=False, repeat='context' if context_repeat else 'off'), available_actions=actions)

    def duplicate_queue():
        send(connection, dict(type='queue_changed', previous=previous, upcoming=upcoming))

    duplicate_state(position=480, volume=41)
    while True:
        opcode, request = read_frame(reader)
        if opcode == 8:
            break
        if opcode == 9:
            send(connection, request, opcode=10)
            continue
        command = json.loads(request)['command']
        if command == 'get_state':
            duplicate_state(position=0 if moved else 480, volume=53 if moved else 41)
        elif command == 'get_queue':
            duplicate_queue()
        elif command == 'skip_next':
            assert not moved, 'Unexpected or duplicate skip'
            if mode not in ('late-ack', 'no-ack'):
                send(connection, dict(type='command_result', command=command))
            moved = True
            upcoming = successor
            send(connection, dict(type='playback_changed', status='buffering'))
            duplicate_queue()
            if mode != 'no-buffering':
                duplicate_state(status='buffering')
            send(connection, dict(type='playback_changed', status='playing'))
            duplicate_state()
            if mode == 'late-ack':
                send(connection, dict(type='command_result', command=command))
            elif mode == 'no-ack':
                send(connection, dict(type='command_result', command='skip_prev'))
            send(connection, dict(type='volume_changed', volume=62))
        elif command == 'pause' and mode == 'repeat-context-rotation':
            assert moved, 'Pause reached the original queue occurrence'
            send(connection, dict(type='command_result', command=command))
        else:
            raise AssertionError(f'Unexpected command: {command}')
elif name.startswith('ws-previous-from-start-'):
    mode = name.removeprefix('ws-previous-from-start-')
    ack_first = mode in ('queue-ack-first', 'buffering-ack-first')
    buffering_successor = mode in ('buffering-ack-first', 'buffering-state-first')
    queue_mode = ack_first or mode in ('queue-state-first', 'buffering-state-first')
    context = dict(uri='spotify:album:' + 'c' * 22)
    successor = dict(uri='spotify:track:' + 'b' * 22)
    initial_state = 'playing' if mode == 'playing' else 'paused'
    cue_sent = False
    previous_sent = False
    queue_test_completed = False

    def state(*, at_start=False, after_previous=False):
        position = anchor(0 if at_start else 3500)
        position['speed'] = int(initial_state == 'playing')
        if at_start and mode == 'stale-cue':
            position['timestamp_ms'] -= 1000
        available = dict(seek={}, skip_prev={})
        if at_start and mode == 'no-action':
            del available['skip_prev']
        if after_previous:
            available['skip_next'] = {}
        output_context = dict(uri='spotify:album:' + 'd' * 22) if at_start and mode == 'wrong-context' else context
        item = {'item': successor} if after_previous else {}
        status = 'buffering' if after_previous and buffering_successor else initial_state
        playback(connection, status=status, position=position, context=output_context,
                 available_actions=available, **item)

    def queue(rows):
        send(connection, dict(type='queue_changed', previous=[], upcoming=rows))

    def verify_successor_queue():
        if ack_first:
            send(connection, dict(type='command_result', command='skip_prev'))
            # Align the complete successor just after a heartbeat. The next
            # regular 250 ms tick must not be what opens the successor queue.
            for sample in range(2):
                assert select.select([connection], [], [], .5)[0], 'Expected state heartbeat'
                opcode, pending = read_frame(reader)
                assert opcode == 1 and json.loads(pending)['command'] == 'get_state', 'A queued read targeted the outgoing item'
                if sample == 0:
                    state(at_start=True)
        send(connection, dict(type='track_changed', item=successor))
        queue([dict(uid='partial', source='context', item=dict(uri='spotify:track:' + 'z' * 22))])
        state(at_start=True, after_previous=True)
        if not ack_first:
            assert not select.select([connection], [], [], .07)[0], 'Successor queue was requested before the Previous ACK'
            send(connection, dict(type='command_result', command='skip_prev'))
        deadline = time.monotonic() + .17
        while True:
            assert select.select([connection], [], [], max(0, deadline - time.monotonic()))[0], 'Expected an immediate successor queue request'
            opcode, pending = read_frame(reader)
            assert opcode == 1
            command = json.loads(pending)['command']
            if command == 'get_queue':
                queue([dict(uid='next', source='context', item=dict(uri='spotify:track:' + 'a' * 22))])
                return
            assert command == 'get_state', f'Unexpected command: {command}'
            state(at_start=True, after_previous=True)

    state()
    while True:
        opcode, request = read_frame(reader)
        if opcode == 8:
            break
        command = json.loads(request)
        if command['command'] == 'get_state':
            state(at_start=cue_sent, after_previous=queue_test_completed)
        elif command['command'] == 'get_queue':
            queue([])
        elif command['command'] == 'seek':
            assert not cue_sent and command['position_ms'] == 0, 'Must seek to zero exactly once'
            cue_sent = True
            if mode in ('early-cue', 'no-seek-ack'):
                # A fresh clock before the Seek ACK does not authorize Previous.
                state(at_start=True)
                send(connection, dict(type='command_result', command='skip_prev'))
                ready = select.select([connection], [], [], .08)[0]
                if ready:
                    opcode, pending = read_frame(reader)
                    assert opcode == 1 and json.loads(pending)['command'] in ('get_state', 'get_queue'), 'Previous reached Soloist before Seek ACK'
            if mode != 'no-seek-ack':
                send(connection, dict(type='command_result', command='seek'))
        elif command['command'] == 'skip_prev':
            assert cue_sent and not previous_sent and (queue_mode or mode in ('early-cue', 'playing', 'paused', 'no-skip-ack'))
            previous_sent = True
            if queue_mode:
                verify_successor_queue()
                queue_test_completed = True
            elif mode != 'no-skip-ack':
                send(connection, dict(type='command_result', command='skip_prev'))
        else:
            raise AssertionError(f"Unexpected command: {command['command']}")
elif name.startswith('ws-option-control-'):
    mode = name.removeprefix('ws-option-control-')
    permitted = dict(shuffle={}, set_repeat={})
    shuffle = False
    repeat = 'context'
    expected = {'shuffle': [('set_shuffle', True)],
                'repeat': [('set_repeat_context', False), ('set_repeat_track', True)]}.get(mode, [])

    def option_state(**changes):
        state = dict(options=dict(shuffle=shuffle, repeat=repeat), available_actions=permitted)
        state.update(changes)
        playback(connection, **state)

    option_state()
    opcode, request = read_frame(reader)
    assert opcode == 1 and json.loads(request)['command'] == 'get_state'
    send(connection, dict(type='options_changed', options=dict(shuffle=False, repeat='off')))
    release_at = time.monotonic() + (.45 if mode == 'expires' else .25)
    while time.monotonic() < release_at:
        if not select.select([connection], [], [], max(0, release_at - time.monotonic()))[0]:
            break
        opcode, request = read_frame(reader)
        assert opcode == 1 and json.loads(request)['command'] in ('get_state', 'get_queue'), 'Options control dispatched without renewed actions'
    if mode == 'device':
        send(connection, dict(type='device_changed', is_active=False))
    if mode == 'auth':
        auth(connection, False)
        auth(connection, True)
    if mode == 'denied':
        option_state(available_actions=dict(shuffle={}))
        # Later permission to repeat does not change this command's denial.
        time.sleep(.1)
    media = {'item': dict(uri='spotify:track:' + 'b' * 22)} if mode == 'media' else {}
    option_state(**media)
    while True:
        opcode, request = read_frame(reader)
        if opcode == 8:
            break
        if opcode == 9:
            send(connection, request, opcode=10)
            continue
        request = json.loads(request)
        command = request['command']
        if command == 'get_state':
            option_state(**media)
        elif command == 'get_queue':
            send(connection, dict(type='queue_changed', previous=[], upcoming=[]))
        else:
            assert expected and (command, request['enabled']) == expected.pop(0), 'Unexpected or duplicate options control'
            if command == 'set_shuffle':
                shuffle = request['enabled']
            elif command == 'set_repeat_track':
                repeat = 'track'
            send(connection, dict(type='command_result', command=command))
            option_state()
elif name.startswith('ws-incomplete-relative-'):
    mode = name.removeprefix('ws-incomplete-relative-')
    actions = dict(seek={}, skip_next={}, skip_prev={}, pause={})
    successor = dict(uri='spotify:track:' + 'b' * 22)
    playback(connection, available_actions=actions)
    skipped = paused = False
    resumed = None
    completes = ('complete', 'stale-complete', 'itemless-playing', 'missing-uri-playing', 'malformed-playing', 'empty-uri-slow')
    slow_cases = ('empty-uri-slow', 'empty-uri-expires', 'empty-uri-changes-shape', 'malformed-expires')
    playing_incomplete = ('itemless-playing', 'missing-uri-playing', 'malformed-playing', 'invalid-position-playing') + slow_cases

    def incomplete_state():
        if mode == 'itemless-playing':
            playback(connection, status='playing', item=None, position=anchor(4100))
        elif mode == 'missing-uri-playing':
            playback(connection, status='playing', item=dict(entity_type='track'), position=anchor(4100))
        elif mode == 'malformed-playing':
            playback(connection, status='playing', item=dict(uri='spotify:track:bad'), position=anchor(4100))
        elif mode == 'invalid-position-playing':
            playback(connection, status='playing', item=None, position=None)
        elif mode in slow_cases:
            malformed = mode == 'malformed-expires' or mode == 'empty-uri-changes-shape' and time.monotonic() >= resumed
            playback(connection, status='playing', item=dict(uri='spotify:track:bad' if malformed else ''), position=anchor(4100))
        else:
            playback(connection, status='buffering', item=None, position=None, is_active=mode != 'inactive')

    while True:
        opcode, request = read_frame(reader)
        if opcode == 8:
            break
        assert opcode == 1
        command = json.loads(request)['command']
        if command in ('skip_next', 'skip_prev'):
            assert not skipped, 'Relative transition replayed the skip'
            skipped = True
            resumed = time.monotonic() + (2.2 if mode in slow_cases else .30)
            if mode == 'stale-complete':
                playback(connection, available_actions=actions)
            if mode not in playing_incomplete:
                incomplete_state()
            if mode == 'stale-complete':
                playback(connection, available_actions=actions)
            send(connection, dict(type='command_result', command=command))
        elif command == 'get_state':
            if mode == 'off-lane-malformed-playing':
                playback(connection, status='playing', item=dict(uri='spotify:track:bad'), position=anchor(4100))
            elif not skipped:
                playback(connection, available_actions=actions)
            elif mode in completes and time.monotonic() >= resumed:
                playback(connection, item=successor, status='paused' if paused else 'playing', available_actions=actions)
            else:
                incomplete_state()
        elif command == 'get_queue':
            send(connection, dict(type='queue_changed', previous=[], upcoming=[]))
        elif command == 'pause':
            assert mode in completes and skipped and not paused and time.monotonic() >= resumed
            paused = True
            send(connection, dict(type='command_result', command=command))
        else:
            raise AssertionError(f'Unexpected command: {command}')
elif name.startswith('ws-autoplay-'):
    mode = name.removeprefix('ws-autoplay-')
    current_uri = 'spotify:track:' + 'a' * 22
    next_uri = 'spotify:track:' + 'b' * 22
    album = dict(uri='spotify:album:' + 'c' * 22) if mode.startswith('album-') else None
    item = dict(uri=current_uri, decorations=dict(identity=dict(name='Original track'),
                                                  playback=dict(duration_ms=65000)))
    status, repeat_mode, position = 'playing', 'off', 63700
    enqueues = 0

    def send_autoplay_state():
        clock = anchor(position)
        clock['speed'] = 1 if status == 'playing' else 0
        actions = dict(pause={}, play={}, seek={}, set_repeat={}, add_to_queue={})
        playback(connection, item=item, context=album, status=status, position=clock,
                 options=dict(shuffle=False, repeat=repeat_mode), available_actions=actions)

    send_autoplay_state()
    while True:
        opcode, raw = read_frame(reader)
        if opcode == 8:
            break
        if opcode == 9:
            send(connection, raw, opcode=10)
            continue
        request = json.loads(raw)
        command = request['command']
        if command == 'get_state':
            send_autoplay_state()
        elif command == 'get_queue':
            upcoming = {
                'album-next': [dict(source='context', item=dict(uri=next_uri))],
                'unknown': [dict(source='unknown', item=dict(uri='new-format'))],
            }.get(mode, [])
            send(connection, dict(type='queue_changed', previous=[], upcoming=upcoming))
        elif command == 'add_to_queue':
            assert mode in ('single', 'album-last') and enqueues == 0 and request['uri'] == next_uri
            enqueues += 1
            send(connection, dict(type='command_result', command=command))
            # Leave the queue empty in the fixture to reproduce delayed queue
            # confirmation without allowing a second write.
        elif command == 'seek':
            position = request['position_ms']
            send(connection, dict(type='command_result', command=command))
        elif command == 'pause':
            status = 'paused'
            send(connection, dict(type='command_result', command=command))
        elif command in ('set_repeat_context', 'set_repeat_track'):
            candidate = command.removeprefix('set_repeat_')
            if request['enabled']:
                repeat_mode = candidate
            elif repeat_mode == candidate:
                repeat_mode = 'off'
            send(connection, dict(type='command_result', command=command))
        else:
            raise AssertionError(f'Unexpected autoplay command: {command}')
else:
    command_fixture = name in ('ws-control-fixture', 'ws-control-logout-fixture', 'ws-control-inactive-fixture')
    queue_fixture = name == 'ws-queue-fixture'
    queue_reads, queue_media = 0, 'a'
    status, position, volume, shuffled = 'playing', 1000, 40, False
    command_context = None
    repeat_steps = []

    def send_state():
        if queue_fixture:
            playback(connection, item=dict(uri='spotify:track:' + queue_media * 22, decorations=dict(
                identity=dict(name='Current queue track'), playback=dict(duration_ms=65000))))
            return
        if not command_fixture:
            playback(connection)
            return
        actions = dict(seek={}, skip_next={}, skip_prev={}, shuffle={}, set_repeat={}, add_to_queue={})
        actions['play' if status == 'paused' else 'pause'] = {}
        clock = anchor(position)
        clock['speed'] = 0 if status == 'paused' else 1
        repeat_mode = ['context', 'off', 'track'][len(repeat_steps)]
        playback(connection, status=status, position=clock, volume=volume, available_actions=actions,
                 context=dict(uri=command_context) if command_context else None,
                 options=dict(shuffle=shuffled, repeat=repeat_mode))

    send_state()
    if name == 'ws-reconnect-fixture':
        time.sleep(.1)
        reader.close()
        connection.close()
        connection, reader = handshake()
        auth(connection, True)
        playback(connection)

    while True:
        opcode, data = read_frame(reader)
        if opcode == 9:
            send(connection, data, opcode=10)
        elif opcode == 8:
            break
        else:
            request = json.loads(data)
            command = request['command']
            if command == 'get_state':
                assert request == dict(type='command', command='get_state')
                send_state()
                continue
            if command == 'get_queue':
                assert request == dict(type='command', command='get_queue', limit=10)
                queue_reads += 1
                empty = dict(type='queue_changed', previous=[], upcoming=[])
                if queue_fixture and queue_reads == 1:
                    def row(id, source):
                        return dict(uid='fixture-key', source=source, item=dict(uri='spotify:track:' + id * 22,
                                    decorations=dict(identity=dict(name='Original preview ' + 'x' * 260))))
                    queue = dict(type='queue_changed', previous=[row('p', 'context')] * 12,
                                 upcoming=[row('q', 'queue'), row('q', 'autoplay'), row('r', 'unknown')] * 4)
                    send(connection, queue)
                    queue_media = 'b'
                    send(connection, dict(type='track_changed', item=dict(uri='spotify:track:' + 'b' * 22)))
                    send(connection, queue)  # Old queue during the new track's transition.
                    send_state()
                else:
                    send(connection, empty)
                continue
            assert command_fixture and request['type'] == 'command'
            if command == 'pause':
                # Only the matching receipt may complete the pipe request.
                send(connection, dict(type='command_result', command='seek'))
                status = 'paused'
            elif command == 'play':
                if 'uri' in request:
                    assert request['uri'] == 'spotify:album:' + 'd' * 22
                    if name != 'ws-control-fixture':
                        if name == 'ws-control-logout-fixture':
                            auth(connection, False)
                        else:
                            send(connection, dict(type='auth_state', logged_in=True, is_active=False))
                        try:
                            send(connection, dict(type='command_result', command=command))
                        except (BrokenPipeError, ConnectionResetError):
                            pass
                        continue
                    auth(connection, True)
                    send(connection, dict(type='auth_state', logged_in='malformed', is_active=False))
                    send(connection, dict(type='error', message='Unrelated get_queue error'))
                    command_context = request['uri']
                    position = 0
                status = 'playing'
            elif command == 'seek':
                assert request['position_ms'] == 4321
                position = 4321
            elif command == 'set_volume':
                assert request['volume'] == 72
                volume = 72
            elif command.startswith('set_repeat_'):
                repeat_steps.append((command, request['enabled']))
                assert repeat_steps == [('set_repeat_context', False), ('set_repeat_track', True)][:len(repeat_steps)]
            elif command == 'add_to_queue':
                assert request['uri'] == 'spotify:track:' + 'b' * 22
                assert len(repeat_steps) == 2
            elif command == 'set_shuffle':
                assert request['enabled'] is True
                shuffled = True
            elif command == 'skip_next':
                # Leave the dispatch uncertain. The helper must drop this socket.
                continue
            else:
                raise AssertionError(f'Unexpected command: {command}')
            send(connection, dict(type='command_result', command=command))
            send_state()

while True:
    time.sleep(.1)

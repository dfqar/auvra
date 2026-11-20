# Swift and Rust interface

The macOS host and the two Rust libraries are built together. The player and the Frost renderer each have their own C header and ABI version. Check each version before creating its handle. Host calls for a given handle have one serial owner. [Player header](../include/player.h) · [Renderer header](../include/still.h)

## Player commands and events

1. Check that `player_abi_version()` matches `PLAYER_ABI_VERSION` and call `player_create(&handle)`. The coordinator runs on its own worker. Initial state arrives asynchronously.
2. On the control queue, call `player_send_json(handle, bytes, length)` with length-delimited UTF-8 JSON. Choose a nonzero request ID unique among outstanding requests. The command size limit is 1 MiB.
3. Call `player_event_drain` to read events. With a null or short buffer, `BufferTooSmall` reports the required size and pins that exact event. A successful copy consumes it. Parse the reported length directly. The JSON has no trailing NUL.
4. Match each accepted request to its `command_result`. Update UI from the associated state, catalog and error events. Playback commands can be accepted before a decoder or device produces sound.
5. Stop submitting commands and call `player_destroy` once on the control queue. It joins the worker. Never race destruction with a send or drain.

This seek request uses the same envelope in Swift and Rust.

```json
{"request_id":7,"command":{"type":"seek","position_ms":15000}}
```

| Result | Meaning |
| --- | --- |
| `Ok` on send | The request was admitted. Wait for the corresponding event before treating it as complete. |
| `Busy`, `InvalidArgument` or `Unavailable` on send | The request was rejected. No asynchronous command result is due for it. |
| `Empty` on drain | No event is available at this time. |
| `BufferTooSmall` on drain | Retry with the reported capacity. |
| `Panic` | An internal failure reached the FFI boundary. Stop using that control path. |

The Swift bridge marshals events to the main actor. The coordinator owns queue and provider state. Audio callbacks, PCM samples, GPU buffers and textures stay out of the player ABI. Analysis events contain bounded numeric features. Avoid logging raw command envelopes because authorization commands can contain credentials. [Swift bridge](../platforms/macos/Auvra/Bridge/PlayerBridge.swift) · [Rust entry points](../crates/player-ffi/src/lib.rs)

## Lyrics and rendering

`player_lyrics_parse` is independent of player handles. Call it on a worker with a UTF-8 JSON request containing `format`, `text` and `duration_ms`. A short output buffer returns the required size. Repeating the same input produces the same normalized document. [Lyric parsers](../crates/player-lyrics/src/lib.rs)

The renderer owns its own surface lifecycle. The macOS host attaches its native layer and supplies size, visibility, frame timing, palette and bounded modulation controls. Publish worker callbacks only for the current surface generation. [Architecture](architecture.md) · [Renderer provenance](still-import.md)

After changing an ABI, run `scripts/cargo.sh xtask headers`, include the updated headers, build with `scripts/build-app.sh` and run `scripts/smoke-bridge.sh` for player changes. [Contributor guide](../CONTRIBUTING.md)

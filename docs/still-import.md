# Frost in Auvra

Auvra's artwork lighting uses part of Still 2.2.0, imported on 22 October 2026. The four Rust crates below and two adapted native host files retain their origin and the Still MIT notice. Everything required to build this version of Frost lives in the player repository. [Still copyright and permission notice](../LICENSES/Still-MIT.txt)

| Included part | Use in the player |
| --- | --- |
| [still-core](../crates/still-core) | Frame timing, render scheduling and quality, ambient colours, modulation and input signals. |
| [still-scenes](../crates/still-scenes) | Frost's scene, shader, parameters and render targets. |
| [still-renderer](../crates/still-renderer) | Metal-backed wgpu surface and compositor for drawing Frost in a Mac window. |
| [still-ffi](../crates/still-ffi) | Static C bridge for renderer lifetime, surfaces, input, ambient state and diagnostics used by the Swift host and its checks. |
| [EngineController and MetalHostView](../platforms/macos/Auvra/Still) | Serial native calls, AppKit lifecycle, frame scheduling and macOS appearance integration. |

Auvra selects Frost for Now Playing, the compact player bar and the artist banner. Its host sets three artwork colours before the first frame, smooths palette changes and can supply short-lived music controls from the player. Each surface has an independent generation. Hiding the host, replacing the surface or losing fresh input clears or suspends the relevant work. The visual engine has no dependency on the Spotify catalogue, player queue or credentials.

This source edition selects Frost at engine creation and exposes it as the only scene. The standalone application, other scene implementations, their shaders, the gallery example and Windows host are outside this repository. Auvra carries its own generated renderer header. The standalone Still SDK and its applications should use their corresponding upstream headers.

[The import record](still-import.json) gives SHA-256 values recorded for the original imported files that have counterparts here. The values describe upstream input on the import date. The host, crate manifests, shader and bridge have since been adapted for Auvra. Files written specifically for Auvra after import have no upstream checksum in that record.

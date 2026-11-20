# Contributing to Auvra

Build the app on Apple Silicon using the instructions in the [README](README.md). A local file is enough to work on playback, the library, timed lyrics and the interface. The Spotify tests provide their own credentials and service replies.

## Where changes belong

SwiftUI and AppKit own windows, system media controls and Keychain. Views work through the shared player store. Rust owns playback, the queue, persistence and provider workers. The audio callback copies into preallocated buffers. Decoding, FFT and filesystem or network access belong on workers. The Frost renderer accepts colours and motion controls. [Architecture](docs/architecture.md) · [Frost provenance](docs/still-import.md)

## Before a pull request

Run the checks for the parts you touched from the repository root.

| Change | Check |
| --- | --- |
| Rust, including providers or the renderer | `scripts/cargo.sh xtask check-rust` |
| Native app, generated headers or linking | `AUVRA_SIGN_IDENTITY=- scripts/build-app.sh` |
| Swift/Rust command and event handling | `scripts/smoke-bridge.sh` |
| Account, OAuth or helper supervision | `scripts/smoke-spotify.sh` |
| SwiftUI, lyric rendering or artwork | `scripts/smoke-ui.sh` |
| Local files, bookmarks or playback restore | `scripts/smoke-library.sh` and `scripts/smoke-transport.sh` |
| Renderer lifecycle or music-responsive lighting | `scripts/smoke-still.sh` and `scripts/smoke-modulation.sh` |
| Distribution notices | `python3 scripts/test-license-packaging.py` |

The native smoke suites require a logged-in macOS session. Some open windows or play quiet, generated audio. The renderer test reports exit code 2 when the window manager cannot establish a required visibility condition. Regenerate C headers with `scripts/cargo.sh xtask headers` when the ABI changes and include the updated headers in the same commit.

For a playback issue, include macOS and Xcode versions, the source (local file, this Mac's Soloist or external Connect), output type, steps, and what you heard or saw. Use original or redistributable fixtures when sharing media. Strip user paths, track history, tokens and callback URLs from diagnostics. Process listings and crash reports can contain the Soloist key in child-process arguments.

Auvra contributions use the [MIT license](LICENSE). Keep existing third-party notices with the files they cover. Security reports containing sensitive information should go privately to the maintainer using the Git commit contact.

# Auvra development

Read [architecture](docs/architecture.md) before changing subsystem boundaries. Follow the change-specific checks in [CONTRIBUTING.md](CONTRIBUTING.md). Build for Apple Silicon macOS from the pinned Rust workspace and Xcode project.

- Views use the shared PlayerStore. Swift owns Keychain, platform windows and system media controls. Rust owns playback coordination, queue state and provider workers.
- Restored selections remain paused until explicit Play. A demo, an unplayed restore and an external Connect device must never claim the system media session.
- Handle credentials on the host. Never log incoming credential commands, account tokens, child-process arguments or OAuth callbacks. Local and provider fixtures use isolated state.
- Audio callbacks may only move preallocated data and update lock-free counters. Decode, FFT, file IO, logging and Swift calls stay off the callback. No PCM, GPU buffers or textures cross the public C ABI.
- If working on owned Soloist capture or experimental spatial output, preserve the fresh media and device lease, PID and parent chain, binary signature and generation checks. Never broaden the tap to other processes, external playback, microphone or system mix.
- Keep only the Frost parts of Still used by Auvra. Preserve the Still MIT notice and document changes in [the import notes](docs/still-import.md). Keep GPU preparation off MainActor and bound rendering to the lifetime and visibility of the native surface.
- Keep toolchains, generated products, credentials, user media and runtime downloads outside source control. Distribute dependency notices with the archive and check generated headers into Git when their ABI changes.

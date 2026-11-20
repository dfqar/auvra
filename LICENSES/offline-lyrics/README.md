# Optional offline lyrics translation

This pack uses the Firefox translation engine and two Mozilla `base-memory` 3.1
model pairs. English can be translated to Russian. Japanese can be translated
to English and can reach Russian through English. The optional personal archive
adds the separate Auvra lyrics helper and approximately 95 MB of unpacked assets.

The Mozilla JS, WASM, and model assets are available under MPL 2.0. The included
license applies to those assets. The original notices for Marian, Intgemm,
SentencePiece, ssplit-cpp and Emscripten are included alongside it.

- [Firefox glue, integration patches and build instructions at the exact source revision](https://github.com/mozilla-firefox/firefox/tree/48d55cf7ec80093903e2ef7f58b61a84a22ef716/toolkit/components/translations/bergamot-translator)
- [Mozilla engine source and build dependencies at the exact engine revision](https://github.com/mozilla/translations/tree/eea6e5a80aa4ddd86d9cc35ce9a65b79aa3ab96d/inference)
- [Engine source checkout, with recursive submodule instructions](https://github.com/mozilla/translations/blob/eea6e5a80aa4ddd86d9cc35ce9a65b79aa3ab96d/inference/scripts/build-wasm.py)
- [Mozilla model source, training pipeline and registry](https://github.com/mozilla/translations)
- [Mozilla confirmation of MPL 2.0 for the released model files](https://github.com/mozilla/translations/pull/1454)

The bundled `Contents/Resources/OfflineLyrics/manifest.json` identifies the exact artifacts, their
upstream locations, and their hashes. Mozilla assets are unmodified.

## Preparing the personal archive

Download the eight assets listed in [manifest.json](manifest.json) separately
into an ignored directory under `build/`. For each `.zst` attachment verify
`zstd_sha256` before decompression. Save the raw result under `input` when present
or under the entry name otherwise. The prepare script verifies each decoded size
and SHA-256, then writes a new directory with the canonical asset names.

```sh
python3 scripts/prepare-offline-lyrics.py build/offline-lyrics-input build/offline-lyrics-pack
AUVRA_SIGN_IDENTITY=- AUVRA_OFFLINE_LYRICS_PACK="$PWD/build/offline-lyrics-pack" scripts/cargo.sh xtask dist local
```

The archive verifies the prepared files again before signing and records the
manifest checksum in `build-info.txt`. Its name includes `local-offline-lyrics`.
The normal build and archive commands use their existing file list. Both stages
work from local inputs. The Developer ID archive rejects the optional pack.

# Packaging Auvra for macOS

Build from a Git checkout with at least one commit on an Apple Silicon Mac. The archive task records the commit and produces a ZIP, a matching SHA-256 file and `build-info.txt` under `build/distribution/`. For a development build you can open directly, follow the [README](../../README.md).

## Personal archive

```sh
AUVRA_SIGN_IDENTITY=- scripts/cargo.sh xtask dist local
```

This produces an ad-hoc signed app for your own Mac. Staged, unstaged and non-ignored untracked files count as changes. A local archive built from a changed checkout has a `-dirty` suffix. The task checks for source changes again before completing and preserves existing output directories by refusing to overwrite them. Commit the inputs for a build you want someone else to reproduce.

The ZIP contains Auvra, its playback helper, bundle metadata, applicable resources, the MIT notices for Auvra and Still, and dependency notices. The packager checks dependency archives against the lockfile and includes corresponding source archives for MPL-covered Symphonia crates. Each user installs the Spotify Soloist runtime separately. Auvra signs the nested helper before the app and verifies signatures again after unpacking. [Archive implementation](../../xtask/src/dist.rs) · [Dependency notice generation](../../LICENSES/README.md) · [Installing Soloist](../spotify.md)

Verify the ZIP from the generated directory containing its `.sha256` file.

```sh
shasum -a 256 -c ./*.zip.sha256
```

Extract the archive with Archive Utility or `ditto -x -k`. Quit any running Auvra with **⌘Q**, then open the extracted copy and play an original local file. Auvra normally reuses its running instance, so opening a second bundle while the first is active would exercise the first binary. An ad-hoc rebuild may prompt for macOS Keychain permission again.

For a personal archive with a separately obtained offline translation pack, set `AUVRA_OFFLINE_LYRICS_PACK` to the absolute path of the prepared directory and use the same `dist local` command. The packager verifies the manifest and signs the additional translation helper. [Offline pack, provenance and supported languages](../../LICENSES/offline-lyrics/README.md)

## Developer ID and notarization

For an independently downloadable macOS binary, start with a clean checkout, including non-ignored untracked files. Obtain a Developer ID Application certificate with its private key in Keychain. Set `AUVRA_DISTRIBUTION_IDENTITY` to that certificate’s SHA-1 fingerprint.

```sh
scripts/cargo.sh xtask dist developer-id
```

The task signs with the hardened runtime and a secure timestamp. Its `notary-input` ZIP still requires submission to Apple. Submit that ZIP with `xcrun notarytool submit` and a configured Keychain profile, wait for **Accepted**, then extract it, staple the ticket to `Auvra.app` and validate it with `xcrun stapler validate`. Repack the stapled app with `ditto -c -k --keepParent` and compute a fresh checksum. Test the final ZIP through a normal browser download on another Mac. Public Spotify-enabled distribution also requires a separate review of third-party service terms. [Apple’s notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow) · [Auvra’s distribution scope](../public-release.md)

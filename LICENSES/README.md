# License notices

Original Auvra code is distributed under MIT by dfqar lab. The imported Frost components retain Still's MIT notice. Both notices are copied to `Contents/Resources/License Notices/` in the app. [Auvra license](../LICENSE) · [Still license](Still-MIT.txt) · [Import record](../docs/still-import.md)

For a distribution archive, `scripts/cargo.sh xtask dist local` also writes a dependency inventory and notices to the `Dependencies` subdirectory. It follows the normal runtime dependencies of both static FFI libraries for `aarch64-apple-darwin`. The packaging tool reads crates from the Cargo cache and verifies each archived package against its hash in `Cargo.lock`. It includes notices from the selected Rust sysroot and the full corresponding source packages for the MPL-covered Symphonia components.

The [registry](runtime-notices.json) records license expressions, upstream locations and checksums for reviewed supplements. Some published crates declare a license in their manifest but omit its text. The [supplements](supplements) directory supplies the corresponding upstream notices for those packages. An unreviewed license, missing notice or mismatched checksum stops archive creation.

To check the packager, run `python3 scripts/test-license-packaging.py`. The optional offline translation pack has its own source references, asset checksums and notices. [Offline translation materials](offline-lyrics/README.md)

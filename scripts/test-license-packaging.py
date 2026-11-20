#!/usr/bin/env python3
"""Regression fixtures for the distribution's dependency notices and sources."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("license_packaging", Path(__file__).with_name("package-licenses.py"))
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)


class LicensePackagingTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.output = self.root / "output"
        self.sysroot = self.root / "sysroot"
        rust = self.sysroot / "share/doc/rust"
        (rust / "licenses").mkdir(parents=True)
        (rust / "COPYRIGHT-library.html").write_text("<p>Original standard library notice</p>")
        (rust / "licenses/MIT.txt").write_text("Original library license fixture")
        self.crate_root = self.root / "cargo/registry/src/original-registry/original-codec-1.0.0"
        self.crate_root.mkdir(parents=True)
        self.archive = self.root / "cargo/registry/cache/original-registry/original-codec-1.0.0.crate"
        self.archive.parent.mkdir(parents=True)
        self.package = self.dependency("original-codec")
        self.package.update(version="1.0.0", license="MPL-2.0", manifest_path=str(self.crate_root / "Cargo.toml"),
                            authors=["Original fixture author"], repository="https://example.invalid/original")
        self.metadata = {"packages": [self.workspace("player-ffi"), self.workspace("still-ffi"), self.package],
                         "workspace_members": ["player-ffi", "still-ffi"], "resolve": {"nodes": [
                             self.node("player-ffi", ["original-codec"]), self.node("still-ffi", []),
                             self.node("original-codec", [])]}}
        (self.root / "LICENSES").mkdir()
        self.rules = {"target": packaging.TARGET, "license_expressions": ["MIT", "MPL-2.0"], "supplements": {}}
        self.write_rules()
        self.write_archive({"LICENSE": b"Original codec license fixture", "src/lib.rs": b"// Original source fixture"})

    def dependency(self, name, kind="lib"):
        return {"id": name, "name": name, "version": "1.0.0", "source": packaging.REGISTRY,
                "targets": [{"kind": [kind]}]}

    def workspace(self, name):
        return dict(self.dependency(name), source=None)

    def node(self, name, dependencies):
        return {"id": name, "deps": [{"pkg": dep, "dep_kinds": [{"kind": None}]} for dep in dependencies]}

    def write_rules(self):
        (self.root / "LICENSES/runtime-notices.json").write_text(json.dumps(self.rules))

    def write_archive(self, files):
        with tarfile.open(self.archive, "w:gz") as output:
            for name, data in files.items():
                member = tarfile.TarInfo("original-codec-1.0.0/" + name)
                member.size = len(data)
                output.addfile(member, io.BytesIO(data))
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        (self.root / "Cargo.lock").write_text(
            'version = 4\n\n[[package]]\nname = "original-codec"\nversion = "1.0.0"\n'
            'source = "' + packaging.REGISTRY + '"\nchecksum = "' + digest + '"\n')

    def stage(self):
        return packaging.stage_notices(self.root, self.output, self.metadata, self.sysroot, "Original compiler fixture")

    def test_sources_and_notices_match_the_published_package(self):
        self.assertEqual(self.stage(), 1)
        index = json.loads((self.output / "packages.json").read_text())
        record = index["packages"][0]
        self.assertEqual((self.output / record["source_archive"]).read_bytes(), self.archive.read_bytes())
        self.assertEqual((self.output / record["notices"][0]["path"]).read_bytes(), b"Original codec license fixture")
        self.assertEqual(record["authors"], ["Original fixture author"])
        self.assertEqual((self.output / "Cargo.lock").read_bytes(), (self.root / "Cargo.lock").read_bytes())
        self.assertNotIn(str(self.root), (self.output / "packages.json").read_text())

    def test_corrupt_archive_and_missing_notice_never_publish_a_partial_bundle(self):
        self.archive.write_bytes(b"Corrupt published archive fixture")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            self.stage()
        self.assertFalse(self.output.exists())
        self.write_archive({"src/lib.rs": b"// Source without a license fixture"})
        with self.assertRaisesRegex(ValueError, "No license notice"):
            self.stage()
        self.assertFalse(self.output.exists())

    def test_supplement_is_checked_and_preserves_its_provenance(self):
        self.write_archive({"src/lib.rs": b"// Source with a declared license fixture"})
        directory = self.root / "LICENSES/supplements"
        directory.mkdir()
        data = b"Original supplementary notice fixture"
        (directory / "notice.txt").write_bytes(data)
        self.rules["supplements"]["original-codec@1.0.0"] = {
            "license": "MPL-2.0", "declaration": "https://example.invalid/declaration", "note": "Original provenance",
            "files": [{"file": "notice.txt", "source": "https://example.invalid/license",
                       "sha256": hashlib.sha256(data).hexdigest()}]}
        self.write_rules()
        (directory / "notice.txt").write_bytes(b"Changed notice fixture")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            self.stage()
        self.assertFalse(self.output.exists())
        (directory / "notice.txt").write_bytes(data)
        self.stage()
        record = json.loads((self.output / "packages.json").read_text())["packages"][0]
        self.assertEqual(record["supplement_note"], "Original provenance")
        with self.assertRaisesRegex(ValueError, "already exists"):
            self.stage()

    def test_both_ffi_roots_include_runtime_dependencies_without_build_or_macro_branches(self):
        self.metadata["packages"] += [self.dependency("visual-runtime"), self.dependency("build-tool"),
                                      self.dependency("macro", "proc-macro"), self.dependency("macro-runtime")]
        self.metadata["resolve"]["nodes"] += [self.node("visual-runtime", []), self.node("build-tool", []),
                                               self.node("macro", ["macro-runtime"]), self.node("macro-runtime", [])]
        self.metadata["resolve"]["nodes"][0]["deps"] += [
            {"pkg": "build-tool", "dep_kinds": [{"kind": "build"}]},
            {"pkg": "macro", "dep_kinds": [{"kind": None}]}]
        self.metadata["resolve"]["nodes"][1] = self.node("still-ffi", ["visual-runtime", "original-codec"])
        self.assertEqual([p["name"] for p in packaging.runtime_packages(self.metadata)],
                         ["original-codec", "visual-runtime"])

    def test_source_archive_with_a_parent_path_is_rejected(self):
        self.write_archive({"../LICENSE": b"Original invalid path fixture"})
        with self.assertRaisesRegex(ValueError, "Invalid license input path"):
            self.stage()
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()

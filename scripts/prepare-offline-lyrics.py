#!/usr/bin/env python3
"""Prepare or stage the pinned Firefox lyrics translation pack without network access."""

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import tempfile


NOTICES = Path(__file__).resolve().parent.parent / "LICENSES" / "offline-lyrics"
MANIFEST = "manifest.json"
ASSETS = {"bergamot.js", "bergamot.wasm"} | {
    f"{pair}-{kind}.bin" for pair in ("jaen", "enru") for kind in ("model", "lex", "vocab")
}
UPSTREAM_NOTICES = {"MPL-2.0.txt", "Marian-MIT.txt", "Intgemm-MIT.txt", "SentencePiece-Apache-2.0.txt",
                    "Ssplit-LICENSE.txt", "Emscripten-LICENSE.txt"}


@contextmanager
def checked_input(path, size):
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size != size:
            raise ValueError(f"Expected a regular file of {size} bytes: {path}")
        yield source


def copy_verified(source_path, destination, entry):
    digest = hashlib.sha256()
    remaining = entry["size"]
    with checked_input(source_path, remaining) as source, destination.open("xb") as output:
        while remaining:
            block = source.read(min(1024 * 1024, remaining))
            if not block:
                break
            output.write(block)
            digest.update(block)
            remaining -= len(block)
        if remaining or source.read(1) or digest.hexdigest() != entry["sha256"]:
            raise ValueError(f"Offline lyrics asset checksum mismatch: {source_path}")


def assemble(source, destination, stage_notices=None):
    pinned = (NOTICES / MANIFEST).read_bytes()
    manifest = json.loads(pinned)
    files = manifest["files"]
    notices = manifest["notices"]
    if manifest["version"] != 1 or files.keys() != ASSETS or notices.keys() != UPSTREAM_NOTICES:
        raise ValueError("Offline lyrics manifest does not match the expected pack layout")
    if source.is_symlink() or not source.is_dir():
        raise ValueError("Offline lyrics input must be a directory")
    if stage_notices is not None:
        if {entry.name for entry in source.iterdir()} != ASSETS | {MANIFEST}:
            raise ValueError("Prepared offline lyrics pack has missing or unexpected files")
        with checked_input(source / MANIFEST, len(pinned)) as submitted:
            if submitted.read(len(pinned) + 1) != pinned:
                raise ValueError("Prepared offline lyrics manifest differs from the pinned manifest")
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"Offline lyrics output already exists: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".offline-lyrics-", dir=destination.parent) as temporary:
        pack = Path(temporary) / "pack"
        pack.mkdir()
        for name, entry in files.items():
            input_name = name if stage_notices is not None else entry.get("input", name)
            copy_verified(source / input_name, pack / name, entry)
        (pack / MANIFEST).write_bytes(pinned)
        if stage_notices is not None:
            stage_notices.mkdir(parents=True)
            for name, entry in notices.items():
                copy_verified(NOTICES / name, stage_notices / name, entry)
            shutil.copyfile(NOTICES / "README.md", stage_notices / "README.md")
        pack.rename(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--stage-notices", type=Path, metavar="DIRECTORY", help=argparse.SUPPRESS)
    args = parser.parse_args()
    try:
        assemble(args.source, args.destination, stage_notices=args.stage_notices)
    except (OSError, ValueError) as error:
        parser.exit(1, f"Offline lyrics pack: {error}\n")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Stage notices and source packages from the locked macOS runtime graph."""
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tarfile
import tempfile


TARGET = "aarch64-apple-darwin"
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
HEADERS = {"name", "version", "source", "checksum"}


def lock_packages(text):
    # Cargo-generated v4 package headers use JSON-compatible quoted strings.
    # Dependencies and other TOML values are outside this reader's scope.
    if "version = 4" not in text.split("[[package]]", 1)[0].splitlines():
        raise ValueError("License packaging requires a Cargo v4 lockfile")
    entries = []
    current = None
    for line in text.splitlines():
        if line == "[[package]]":
            current = {}
            entries.append(current)
        elif current is not None and " = " in line:
            key, value = line.split(" = ", 1)
            if key in HEADERS:
                current[key] = json.loads(value)
    return {(p["name"], p["version"], p.get("source")): p for p in entries}


def runtime_packages(metadata):
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {p["id"]: p for p in metadata["resolve"]["nodes"]}
    workspace = set(metadata["workspace_members"])
    roots = [p["id"] for p in packages.values()
             if p["id"] in workspace and p["name"] in {"player-ffi", "still-ffi"}]
    if len(roots) != 2:
        raise ValueError("Both Auvra FFI roots are required for license packaging")
    pending = list(roots)
    seen = set()
    selected = []
    while pending:
        identifier = pending.pop()
        if identifier in seen:
            continue
        seen.add(identifier)
        package = packages[identifier]
        if any("proc-macro" in t["kind"] for t in package["targets"]):
            continue
        if identifier not in workspace:
            if package["source"] != REGISTRY:
                raise ValueError("Unreviewed dependency source for " + package["name"])
            selected.append(package)
        for dependency in nodes[identifier]["deps"]:
            if any(k["kind"] is None for k in dependency["dep_kinds"]):
                pending.append(dependency["pkg"])
    return sorted(selected, key=lambda p: (p["name"], p["version"]))


def relative_path(value):
    path = PurePosixPath(value)
    if path.is_absolute() or not path.parts or ".." in path.parts or "\\" in value:
        raise ValueError("Invalid license input path " + value)
    return path


def checked_bytes(path, checksum):
    if path.is_symlink() or not path.is_file():
        raise ValueError("License input must be a regular file: " + str(path))
    data = path.read_bytes()
    if hashlib.sha256(data).hexdigest() != checksum:
        raise ValueError("License input checksum mismatch: " + str(path))
    return data


def write_notice(output, relative, data, origin):
    if not data.strip():
        raise ValueError("Empty license notice: " + relative)
    path = output / relative_path(relative)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return {"path": relative, "sha256": hashlib.sha256(data).hexdigest(), "origin": origin}


def package_notices(package, locked, rules, root, output):
    name = package["name"]
    version = package["version"]
    prefix = name + "-" + version
    relative_path(prefix)
    license_expression = package.get("license")
    if license_expression not in rules["license_expressions"]:
        raise ValueError("Unreviewed license expression for " + prefix)
    crate_root = Path(package["manifest_path"]).parent
    archive = crate_root.parents[2] / "cache" / crate_root.parent.name / (prefix + ".crate")
    source = checked_bytes(archive, locked["checksum"])
    notices = []
    with tarfile.open(fileobj=io.BytesIO(source), mode="r:gz") as published:
        for member in published:
            path = relative_path(member.name)
            if path.parts[0] != prefix:
                raise ValueError("Unexpected source package root for " + prefix)
            if member.isdir():
                continue
            if not member.isfile():
                raise ValueError("Source package contains a link or special file: " + member.name)
            relative = PurePosixPath(*path.parts[1:])
            if relative.name.lower().startswith(("license", "notice", "copying", "copyright")):
                notices.append(write_notice(output, "packages/" + prefix + "/" + str(relative),
                                            published.extractfile(member).read(),
                                            "Published crate, SHA-256 " + locked["checksum"]))
    supplement = rules["supplements"].get(name + "@" + version)
    if supplement:
        if supplement["license"] != license_expression:
            raise ValueError("Supplement no longer matches the license of " + prefix)
        for entry in supplement["files"]:
            path = root / "LICENSES" / "supplements" / relative_path(entry["file"])
            data = checked_bytes(path, entry["sha256"])
            notices.append(write_notice(output, "packages/" + prefix + "/supplements/" + entry["file"],
                                        data, entry["source"]))
    if not notices:
        raise ValueError("No license notice found for " + prefix)
    record = {"name": name, "version": version, "license": license_expression,
              "authors": package.get("authors", []), "repository": package.get("repository"),
              "crate_sha256": locked["checksum"], "notices": notices}
    if supplement:
        record["supplement_declaration"] = supplement["declaration"]
        record["supplement_note"] = supplement.get("note", "")
    if license_expression == "MPL-2.0":
        destination = "sources/" + prefix + ".tar.gz"
        path = output / destination
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(source)
        record["source_archive"] = destination
    return record


def stage_notices(root, destination, metadata, sysroot, rust_version):
    if destination.exists() or destination.is_symlink():
        raise ValueError("License destination already exists: " + str(destination))
    lock_data = (root / "Cargo.lock").read_bytes()
    locked = lock_packages(lock_data.decode("utf-8"))
    rules = json.loads((root / "LICENSES" / "runtime-notices.json").read_text())
    if rules["target"] != TARGET:
        raise ValueError("License rules do not match the archive target")
    packages = runtime_packages(metadata)
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".licenses-", dir=destination.parent) as temporary:
        output = Path(temporary) / "Dependencies"
        output.mkdir()
        records = []
        for package in packages:
            entry = locked[(package["name"], package["version"], package["source"])]
            records.append(package_notices(package, entry, rules, root, output))
        rust = sysroot / "share" / "doc" / "rust"
        shutil.copyfile(rust / "COPYRIGHT-library.html", output / "Rust Standard Library.html")
        shutil.copytree(rust / "licenses", output / "licenses")
        (output / "Cargo.lock").write_bytes(lock_data)
        inventory = {"target": TARGET, "cargo_lock_sha256": hashlib.sha256(lock_data).hexdigest(),
                     "rust_compiler": rust_version, "packages": records}
        (output / "packages.json").write_text(json.dumps(inventory, indent=2, ensure_ascii=False) + "\n")
        (output / "README.txt").write_text(
            "Auvra dependency notices\n\n"
            "packages.json identifies the locked macOS arm64 runtime dependencies and their notices.\n"
            "The packages folder preserves the published license files and reviewed supplements.\n"
            "Declared authors are copied from each package's Cargo metadata.\n"
            "Rust Standard Library.html contains the toolchain's library notices.\n\n"
            "The sources folder contains the corresponding unmodified Symphonia source packages\n"
            "under MPL-2.0. Each .tar.gz is the original published crate with its Cargo.lock checksum.\n"
            "Extract an individual package with tar -xzf <package>.tar.gz.\n"
            "Its source code, original notices and package manifests are included.\n")
        os.rename(output, destination)
    return len(records)


def main():
    root = Path(__file__).resolve().parents[1]
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--offline", "--format-version", "1", "--filter-platform", TARGET], cwd=root))
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    rust_version = subprocess.check_output(["rustc", "--version", "--verbose"], text=True).strip()
    count = stage_notices(root, Path(sys.argv[1]), metadata, sysroot, rust_version)
    print("Packaged notices for " + str(count) + " locked runtime dependencies")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, tarfile.TarError, subprocess.CalledProcessError) as error:
        sys.exit("License packaging failed: " + str(error))

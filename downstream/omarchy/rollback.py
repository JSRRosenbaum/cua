#!/usr/bin/env python3
"""Retain/check the exact known-good ALPM rollback set. Never transact live."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

PROFILE = Path("/usr/share/cua-hyprland-plugin/PROFILE.json")
CURRENT_DRIVER = "0.28.2-1"
CURRENT_PLUGIN = "0.26.1-6"


def require(value, message):
    if not value:
        raise ValueError(message)


def hash_file(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(*arguments):
    return subprocess.check_output(arguments, text=True, stderr=subprocess.PIPE).strip()


def package_versions(profile):
    return {"hyprland": profile["hyprland"]["package_version"],
            **profile["runtime"]["packages"],
            "cua-driver-bin": CURRENT_DRIVER,
            "cua-hyprland-plugin": CURRENT_PLUGIN}


def source_package(cachedir, name, version):
    matches = list(cachedir.glob(f"{name}-{version}-*.pkg.tar.zst"))
    require(len(matches) == 1 and not matches[0].is_symlink(),
            f"exact package archive required: {name}={version}")
    archive = matches[0]
    require(run("pacman", "-Qp", str(archive)) == f"{name} {version}",
            f"archive metadata changed: {archive}")
    signature = archive.with_name(archive.name + ".sig")
    require(signature.is_file() and not signature.is_symlink(),
            f"package signature missing: {signature}")
    run("pacman-key", "--verify", str(signature), str(archive))
    return archive, signature


def snapshot(cachedir, output):
    require(not output.exists(), "rollback set output already exists")
    require(PROFILE.is_file() and not PROFILE.is_symlink(), "installed profile missing")
    profile = json.loads(PROFILE.read_text())
    expected = package_versions(profile)
    for name, version in expected.items():
        require(run("pacman", "-Q", name) == f"{name} {version}",
                f"installed rollback baseline differs: {name}")
    source = {name: source_package(cachedir, name, version)
              for name, version in expected.items()}
    output.mkdir(mode=0o700, parents=True)
    archive_dir = output / "packages"
    archive_dir.mkdir(mode=0o700)
    files = {}
    for archive, signature in source.values():
        for path in (archive, signature):
            destination = archive_dir / path.name
            shutil.copyfile(path, destination)
            destination.chmod(0o600)
            files[path.name] = {"sha256": hash_file(destination), "size": destination.stat().st_size}
    installed_metadata = Path("/usr/share/cua-hyprland-plugin")
    metadata_dir = output / "plugin-metadata"
    metadata_dir.mkdir(mode=0o700)
    metadata = {}
    for path in installed_metadata.iterdir():
        require(path.is_file() and not path.is_symlink(), "unexpected installed plugin metadata")
        destination = metadata_dir / path.name
        shutil.copyfile(path, destination)
        destination.chmod(0o600)
        metadata[path.name] = hash_file(destination)
    binary_hashes = {name: hash_file(Path(path)) for name, path in {
        "hyprland": "/usr/bin/Hyprland", "driver": "/usr/lib/cua-driver/cua-driver",
        "plugin": "/usr/lib/cua/hyprland/cua-hyprland-plugin.so"}.items()}
    record = {"schema": 1, "baseline_packages": expected, "files": files,
              "plugin_metadata": metadata, "installed_binary_sha256": binary_hashes,
              "profile_sha256": hash_file(metadata_dir / "PROFILE.json"),
              "activation": "fresh-login-only; no live plugin unload/replacement"}
    (output / "ROLLBACK.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
    (output / "ROLLBACK.json").chmod(0o600)
    return record


def check(root):
    record = json.loads((root / "ROLLBACK.json").read_text())
    require(record["schema"] == 1, "unsupported rollback manifest")
    packages = record["baseline_packages"]
    profile_dir = root / "plugin-metadata"
    require(hash_file(profile_dir / "PROFILE.json") == record["profile_sha256"],
            "rollback profile changed")
    profile = json.loads((profile_dir / "PROFILE.json").read_text())
    require(packages == package_versions(profile), "rollback package/profile pairing changed")
    require({p.name for p in (root / "packages").iterdir()} == set(record["files"]),
            "rollback package inventory changed")
    for name, item in record["files"].items():
        path = root / "packages" / name
        require(path.is_file() and not path.is_symlink() and
                path.stat().st_size == item["size"] and hash_file(path) == item["sha256"],
                f"rollback artifact changed: {name}")
    for name, version in packages.items():
        source_package(root / "packages", name, version)
    require({p.name for p in profile_dir.iterdir()} == set(record["plugin_metadata"]),
            "plugin metadata inventory changed")
    for name, digest in record["plugin_metadata"].items():
        path = profile_dir / name
        require(path.is_file() and not path.is_symlink() and hash_file(path) == digest,
                f"plugin metadata changed: {name}")
    return record


def cli():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    snap = sub.add_parser("snapshot", help="retain exact current full package set; never install")
    snap.add_argument("--cachedir", type=Path, default=Path("/var/cache/pacman/pkg"))
    snap.add_argument("--output", type=Path, required=True)
    verify = sub.add_parser("check", help="validate retained set and print offline rollback boundary")
    verify.add_argument("--root", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.operation == "snapshot":
            record = snapshot(args.cachedir.resolve(), args.output.absolute())
        else:
            record = check(args.root.resolve())
        print(json.dumps({"packages": record["baseline_packages"],
                          "activation": record["activation"]}, sort_keys=True))
        print("Rollback requires logging out first. Reinstall the retained matching "
              "compositor, ABI runtime, Driver and plugin packages together from a VT/offline "
              "environment; verify the module/profile bytes, then start a new graphical session. "
              "This helper never unloads the active plugin or changes packages.")
    except (ValueError, OSError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"error: {error}\n")


if __name__ == "__main__":
    cli()

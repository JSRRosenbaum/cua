#!/usr/bin/env python3
"""Prepare and verify a local, paired Cua/Hyprland build without activating it."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import shlex
import tarfile


SOURCE_PATHS = ("LICENSE.md", "libs/cua-driver/rust", "libs/cua-driver/hyprland-plugin")
EXCLUDED = ("/build/", "/target/", "/.git/", "/__pycache__/")
ABI_PACKAGES = ("aquamarine", "glibc", "hyprcursor", "hyprgraphics", "hyprlang",
                "hyprutils", "libgcc", "libstdc++", "libxkbcommon", "wayland")
PLUGIN_OPTIONS = {"CUA_HYPRLAND_INPUT": "ON", "CUA_HYPRLAND_TEST_INPUT": "OFF",
                  "CUA_HYPRLAND_INPUT_TRACE": "OFF"}
ROUTING_ENV = {"CPATH", "CPLUS_INCLUDE_PATH", "C_INCLUDE_PATH", "OBJC_INCLUDE_PATH",
               "GCC_EXEC_PREFIX", "COMPILER_PATH", "LIBRARY_PATH", "SDKROOT", "SYSROOT"}
ROUTING_FLAGS = ("-I", "-L", "-B", "-isystem", "-iquote", "-idirafter",
                 "-include", "-imacros", "-isysroot", "--sysroot", "-nostdinc",
                 "-specs", "--specs", "-fplugin", "-wrapper", "-Xpreprocessor",
                 "-Xclang", "-Xlinker", "-rpath", "--rpath")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def verify_build_environment():
    for name, value in os.environ.items():
        routed = name in ROUTING_ENV or name.startswith(("PKG_CONFIG", "PKGCONF")) or (
            name.startswith("CMAKE_") and name != "CMAKE_BUILD_PARALLEL_LEVEL")
        require(not value or not routed, f"build routing environment refused: {name}")
    for name in ("CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS"):
        for flag in shlex.split(os.environ.get(name, "")):
            arguments = flag[4:].split(",") if flag.startswith(("-Wp,", "-Wl,")) else [flag]
            for argument in arguments:
                require(not argument.startswith(("@", *ROUTING_FLAGS)),
                        f"header/toolchain flag override refused: {name}")


def command(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.PIPE).strip()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def digest(path):
    return sha(Path(path).read_bytes())


def encoded(value):
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def regular(path):
    path = Path(path)
    require(path.is_file() and not path.is_symlink(), f"regular file required: {path}")
    return path


def safe_dir(path):
    path = Path(path)
    require(path.is_dir() and not path.is_symlink(), f"real directory required: {path}")
    return path


def git(repo, *args):
    return command("git", "-C", str(repo), *args)


def tracked_sources(repo):
    names = subprocess.check_output(["git", "-C", str(repo), "ls-files", "-z", "--cached",
                                     "--others", "--exclude-standard", "--", *SOURCE_PATHS]).split(b"\0")
    files = {}
    for raw in names:
        if not raw:
            continue
        name = raw.decode()
        require(not name.startswith("/") and ".." not in Path(name).parts,
                "unsafe source path")
        if any(fragment in "/" + name + "/" for fragment in EXCLUDED):
            continue
        path = repo / name
        regular(path)
        files[name] = path.read_bytes()
    require("LICENSE.md" in files and "libs/cua-driver/rust/Cargo.lock" in files and
            "libs/cua-driver/hyprland-plugin/CMakeLists.txt" in files,
            "incomplete Driver/plugin source inventory")
    return files


def manifest_source(repo):
    files = tracked_sources(repo)
    manifest = {"schema": 1, "base_revision": git(repo, "rev-parse", "HEAD"),
                "driver_version": "0.29.1", "files": {name: sha(data) for name, data in sorted(files.items())}}
    return files, manifest


def archive(files):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for name, data in sorted(files.items()):
            entry = tarfile.TarInfo("cua-source/" + name)
            entry.size = len(data)
            entry.mode = 0o644
            entry.mtime = 0
            tar.addfile(entry, io.BytesIO(data))
    compressed = io.BytesIO()
    with gzip.GzipFile(filename="", fileobj=compressed, mode="wb", mtime=0, compresslevel=9) as out:
        out.write(stream.getvalue())
    return compressed.getvalue()

def stage_archive(stage):
    root = safe_dir(stage / "usr")
    inventory = {}
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for path in sorted(root.rglob("*")):
            if path.is_dir() and not path.is_symlink():
                continue
            name = "usr/" + path.relative_to(root).as_posix()
            entry = tarfile.TarInfo("candidate-compositor/" + name)
            entry.mtime = 0
            if path.is_symlink():
                target = os.readlink(path)
                require(target.startswith("/usr/") or
                        (not Path(target).is_absolute() and ".." not in Path(target).parts),
                        f"unsafe staged symlink: {name}")
                entry.type = tarfile.SYMTYPE
                entry.linkname = target
                entry.mode = 0o777
                inventory[name] = {"symlink": target}
                tar.addfile(entry)
            else:
                regular(path)
                data = path.read_bytes()
                entry.size = len(data)
                entry.mode = path.stat().st_mode & 0o777
                inventory[name] = {"sha256": sha(data), "mode": entry.mode}
                tar.addfile(entry, io.BytesIO(data))
    require("usr/bin/Hyprland" in inventory and "usr/include/hyprland/src/plugins/PluginAPI.hpp" in inventory,
            "incomplete staged compositor payload")
    compressed = io.BytesIO()
    with gzip.GzipFile(filename="", fileobj=compressed, mode="wb", mtime=0, compresslevel=9) as out:
        out.write(stream.getvalue())
    return compressed.getvalue(), inventory


def installed_packages():
    return {name: command("pacman", "-Q", name).split(" ", 1)[1] for name in ABI_PACKAGES}


def dependencies(name):
    version = command("pacman", "-Q", name).split(" ", 1)[1]
    descriptor = regular(Path("/var/lib/pacman/local") / f"{name}-{version}" / "desc")
    fields = descriptor.read_text().split("\n%DEPENDS%\n", 1)
    require(len(fields) == 2, f"installed ALPM dependencies missing: {name}")
    result = fields[1].split("\n\n", 1)[0].splitlines()
    require(result and all(re.fullmatch(r"[a-zA-Z0-9+_.:@=<>-]+", item)
                           for item in result), "invalid installed package dependencies")
    return result


def source_tree(root, manifest):
    root = safe_dir(root)
    actual = {}
    for path in root.rglob("*"):
        name = path.relative_to(root).as_posix()
        require(not path.is_symlink(), f"source tree symlink refused: {path}")
        if path.is_dir():
            require(any(key.startswith(name + "/") for key in manifest), f"extra source directory: {path}")
        else:
            actual[name] = digest(regular(path))
    require(actual == manifest, "extracted source tree differs from reviewed archive")


def extracted_stage(root, inventory):
    root = safe_dir(root)
    actual = {}
    for path in root.rglob("*"):
        name = path.relative_to(root).as_posix()
        if path.is_symlink():
            actual[name] = {"symlink": os.readlink(path)}
        elif path.is_dir():
            require(any(key.startswith(name + "/") for key in inventory),
                    f"extra compositor directory: {path}")
        else:
            actual[name] = {"sha256": digest(regular(path)),
                            "mode": path.stat().st_mode & 0o777}
    require(actual == inventory, "extracted compositor tree differs from reviewed stage")




def headers_hash(root):
    root = safe_dir(root)
    files = {}
    for path in root.rglob("*"):
        require(not path.is_symlink(), f"unexpected symlink in compositor headers: {path}")
        if path.is_file():
            files[path.relative_to(root).as_posix()] = digest(path)
    require("src/plugins/PluginAPI.hpp" in files and "src/managers/SeatManager.hpp" in files,
            "candidate plugin headers incomplete")
    return sha(encoded(files))


def plugin_pkgconfig(stage):
    stage = safe_dir(stage)
    pkgconfig = regular(stage / "usr/share/pkgconfig/hyprland.pc")
    text = pkgconfig.read_text()
    require(text.startswith("prefix=/usr/include\n"), "unexpected upstream hyprland.pc prefix")
    return text.replace("prefix=/usr/include\n", f"prefix={stage}/usr/include\n", 1)


def linked_dependencies(binary):
    dynamic = command("readelf", "-d", str(binary))
    needed = set(re.findall(r"Shared library: \[([^]]+)\]", dynamic))
    output = command("ldd", str(binary))
    require("not found" not in output, f"unresolved shared dependency: {binary}")
    resolved = {Path(name).name: path for name, path in
                re.findall(r"^\s*(\S+) => (/\S+) \(0x[0-9a-fA-F]+\)\s*$",
                           output, re.M)}
    require(needed <= resolved.keys(), f"unresolved ELF dependencies: {binary}")
    return {name: Path(path).resolve(strict=True) for name, path in resolved.items()
            if name in needed}


def runtime_path(binary):
    dynamic = command("readelf", "-d", str(binary))
    require("Shared library: [libstdc++.so.6]" in dynamic and
            "Shared library: [libc++.so" not in dynamic,
            "native module must use shared libstdc++")
    runtime = linked_dependencies(binary).get("libstdc++.so.6")
    require(runtime is not None and command("pacman", "-Qoq", str(runtime)) == "libstdc++",
            "native C++ runtime owner mismatch")
    return runtime


def dependency_owners(binary, declared, allow_extra=False):
    allowed = {item.split("=", 1)[0] for item in declared} | set(ABI_PACKAGES)
    owners = {name: command("pacman", "-Qoq", str(path))
              for name, path in linked_dependencies(binary).items()}
    require(allow_extra or set(owners.values()) <= allowed,
            f"candidate ELF adds an undeclared native dependency: {owners}")
    return owners


def compiler_version():
    first = command("g++", "--version").splitlines()[0]
    require(first.startswith("g++ (GCC) "), "candidate compiler must be the reviewed GCC")
    return first.removeprefix("g++ (GCC) ")


def rust_toolchain():
    rustc = regular(command("rustup", "which", "rustc")).resolve(strict=True)
    cargo = regular(command("rustup", "which", "cargo")).resolve(strict=True)
    return {"rustc": command(str(rustc), "--version"),
            "cargo": command(str(cargo), "--version"),
            "rustup_toolchain": os.environ.get("RUSTUP_TOOLCHAIN", ""),
            "rustc_path": str(rustc), "cargo_path": str(cargo),
            "rustc_sha256": digest(rustc), "cargo_sha256": digest(cargo)}


def candidate_profile(compositor, stage, source, archive_bytes, stage_bytes, inventory):
    binary = regular(stage / "usr/bin/Hyprland")
    headers = stage / "usr/include/hyprland"
    revision = git(compositor, "rev-parse", "HEAD")
    info = json.loads(command(str(binary), "--version-json"))
    require(info["commit"] == revision and info["commit"] != "efb50993780079460b0cbed1363e2166a2de1d9f",
            "candidate compositor needs a distinct committed ABI identity")
    require(info["version"] == "0.56.2" and not info["dirty"],
            "compositor version or committed provenance mismatch")
    stock_binary = regular("/usr/bin/Hyprland")
    stock_info = json.loads(command(str(stock_binary), "--version-json"))
    installed_profile_path = regular("/usr/share/cua-hyprland-plugin/PROFILE.json")
    installed_profile = json.loads(installed_profile_path.read_text())
    require(stock_info["commit"] == "efb50993780079460b0cbed1363e2166a2de1d9f" and
            command("pacman", "-Q", "hyprland") == "hyprland 0.56.2-3" and
            digest(stock_binary) == installed_profile["hyprland"]["sha256"] and
            installed_profile["runtime"]["packages"] == installed_packages(),
            "known-good stock compositor/profile required for candidate kit")
    require(info["abiHash"] != stock_info["abiHash"], "candidate compositor must not advertise stock ABI")
    compiler = regular("/usr/bin/g++")
    gcc_version = compiler_version()
    runtime = runtime_path(binary)
    require("GCC: (GNU) " + gcc_version in command("readelf", "-p", ".comment", str(binary)),
            "candidate compositor compiler comment mismatch")
    require(not git(compositor, "status", "--porcelain", "--untracked-files=no", "--", "src", "CMakeLists.txt"),
            "compositor source changed after candidate commit")
    stock = {"abi_hash": stock_info["abiHash"], "binary_sha256": digest(stock_binary),
             "profile_sha256": digest(installed_profile_path),
             "compositor_dependencies": dependencies("hyprland"),
             "driver_dependencies": dependencies("cua-driver-bin")}
    native_owners = dependency_owners(binary, stock["compositor_dependencies"], allow_extra=True)
    allowed = {item.split("=", 1)[0] for item in stock["compositor_dependencies"]} | set(ABI_PACKAGES)
    extra = [command("pacman", "-Q", name).replace(" ", "=", 1)
             for name in sorted(set(native_owners.values()) - allowed)]
    return {"schema": 1, "stock": stock,
            "source": {**source, "archive_sha256": sha(archive_bytes),
                       "package_version": "0.29.1.cua." + sha(archive_bytes)[:12]},
            "compositor": {"revision": revision, "version": info["version"],
                           "package_version": "0.56.2.cua." + revision[:12] + "-1",
                           "abi_hash": info["abiHash"], "binary_sha256": digest(binary),
                           "headers_sha256": headers_hash(headers),
                           "native_dependency_owners": native_owners,
                           "extra_dependencies": extra,
                           "pkgconfig_sha256": sha(plugin_pkgconfig(stage).encode()),
                           "stage_archive_sha256": sha(stage_bytes),
                           "stage_inventory_sha256": sha(encoded(inventory)),
                           "staged_root": str(stage)},
            "compiler": {"path": str(compiler), "sha256": digest(compiler),
                         "gcc_version": gcc_version, "runtime": str(runtime),
                         "runtime_sha256": digest(runtime)},
            "packages": installed_packages(), "plugin_options": PLUGIN_OPTIONS,
            "driver_toolchain": rust_toolchain()}


def verify_builder(kit, extracted=None, candidate_installed=False):
    verify_build_environment()
    kit = safe_dir(kit)
    profile = json.loads(regular(kit / "PROFILE.json").read_text())
    provenance = json.loads(regular(kit / "KIT-PROVENANCE.json").read_text())
    require(digest(kit / "PROFILE.json") == provenance["profile_sha256"] and
            digest(kit / "profile_verify.py") == provenance["verifier_sha256"] and
            digest(Path(__file__)) == provenance["verifier_sha256"] and
            digest(kit / "SOURCE-PROVENANCE.json") == provenance["source_manifest_sha256"],
            "reviewed kit inputs changed")
    require(json.loads(regular(kit / "SOURCE-PROVENANCE.json").read_text()) ==
            {key: value for key, value in profile["source"].items()
             if key not in ("archive_sha256", "package_version")},
            "source manifest and profile disagree")
    require(profile["schema"] == 1 and profile["plugin_options"] == PLUGIN_OPTIONS,
            "unsupported candidate profile")
    require(profile["packages"] == installed_packages(), "ABI runtime package versions changed")
    compiler = profile["compiler"]
    require(digest(compiler["path"]) == compiler["sha256"] and
            compiler_version() == compiler["gcc_version"], "compiler changed")
    require(digest(compiler["runtime"]) == compiler["runtime_sha256"], "C++ runtime changed")
    candidate = profile["compositor"]
    stage = safe_dir(candidate["staged_root"])
    binary = regular(stage / "usr/bin/Hyprland")
    require(digest(binary) == candidate["binary_sha256"], "candidate compositor binary changed")
    info = json.loads(command(str(binary), "--version-json"))
    require(info["commit"] == candidate["revision"] and info["abiHash"] == candidate["abi_hash"] and
            info["abiHash"] != profile["stock"]["abi_hash"],
            "compositor ABI identity changed or matches recorded stock")
    if candidate_installed:
        require(command("pacman", "-Q", "hyprland-cua") ==
                f"hyprland-cua {candidate['package_version']}" and
                digest("/usr/bin/Hyprland") == candidate["binary_sha256"],
                "exact candidate compositor must be installed in the isolated build root")
    else:
        require(digest("/usr/bin/Hyprland") in
                (candidate["binary_sha256"], profile["stock"]["binary_sha256"]),
                "installed compositor differs from recorded stock/candidate")
    require(headers_hash(stage / "usr/include/hyprland") == candidate["headers_sha256"],
            "candidate compositor headers changed")
    require(sha(plugin_pkgconfig(stage).encode()) == candidate["pkgconfig_sha256"],
            "candidate pkgconfig changed")
    if (kit / "hyprland.pc").exists():
        require(digest(regular(kit / "hyprland.pc")) == candidate["pkgconfig_sha256"],
                "actual kit pkg-config file differs from candidate stage")
        pc_env = {**os.environ, "PKG_CONFIG_PATH": str(kit)}
        require(subprocess.check_output(["/usr/bin/pkgconf", "--variable=pcfiledir", "hyprland"],
                                        env=pc_env, text=True).strip() == str(kit.resolve()) and
                subprocess.check_output(["/usr/bin/pkgconf", "--modversion", "hyprland"],
                                        env=pc_env, text=True).strip() == candidate["version"],
                "pkgconf selected a different Hyprland header package")
    require(runtime_path(binary) == Path(compiler["runtime"]) and
            digest(runtime_path(binary)) == compiler["runtime_sha256"],
            "compositor/compiler runtime mismatch")
    require(dependency_owners(binary, profile["stock"]["compositor_dependencies"] +
                              candidate["extra_dependencies"]) ==
            candidate["native_dependency_owners"], "compositor ELF native dependencies changed")
    archive_path = regular(kit / "cua-source.tar.gz")
    require(digest(archive_path) == profile["source"]["archive_sha256"], "source archive changed")
    with tarfile.open(archive_path, "r:gz") as source_archive:
        actual = {}
        for entry in source_archive:
            require(entry.isfile() and entry.name.startswith("cua-source/"), "unsafe archive entry")
            name = entry.name.removeprefix("cua-source/")
            require(name and name not in actual and ".." not in Path(name).parts, "invalid archive inventory")
            actual[name] = sha(source_archive.extractfile(entry).read())
    require(actual == profile["source"]["files"], "source archive manifest mismatch")
    if extracted is not None:
        source_tree(extracted, profile["source"]["files"])
    return profile

def verify_stage(kit, extracted=None):
    profile = json.loads(regular(kit / "PROFILE.json").read_text())
    candidate = profile["compositor"]
    stage = safe_dir(candidate["staged_root"])
    source = regular(kit / "candidate-compositor.tar.gz")
    stage_bytes, inventory = stage_archive(stage)
    require(digest(source) == candidate["stage_archive_sha256"] and
            sha(stage_bytes) == candidate["stage_archive_sha256"] and
            sha(encoded(inventory)) == candidate["stage_inventory_sha256"],
            "candidate compositor stage differs from reviewed package archive")
    if extracted is not None:
        extracted_stage(safe_dir(extracted), inventory)
    binary = regular(stage / "usr/bin/Hyprland")
    info = json.loads(command(str(binary), "--version-json"))
    require(digest(binary) == candidate["binary_sha256"] and
            info["abiHash"] == candidate["abi_hash"] and
            info["commit"] == candidate["revision"] and not info["dirty"],
            "candidate compositor staged ABI differs from reviewed profile")
    require(headers_hash(stage / "usr/include/hyprland") == candidate["headers_sha256"],
            "staged plugin headers changed")
    require(profile["packages"] == installed_packages() and
            digest(runtime_path(binary)) == profile["compiler"]["runtime_sha256"],
            "native runtime changed")
    require(dependency_owners(binary, profile["stock"]["compositor_dependencies"] +
                              candidate["extra_dependencies"]) ==
            candidate["native_dependency_owners"], "staged compositor native dependencies changed")
    return profile

def verify_cmake(build, kit, profile):
    cache = {}
    for line in regular(Path(build) / "CMakeCache.txt").read_text().splitlines():
        match = re.match(r"([^:#/][^:]*):[^=]+=(.*)", line)
        if match:
            require(match[1] not in cache, "duplicate CMake cache entry")
            cache[match[1]] = match[2]
    expected = {**profile["plugin_options"], "BUILD_TESTING": "ON",
                "CUA_HYPRLAND_BUILD_PLUGIN": "ON", "CMAKE_BUILD_TYPE": "Release",
                "CMAKE_GENERATOR": "Ninja", "CMAKE_CXX_COMPILER": profile["compiler"]["path"],
                "CUA_HYPRLAND_EXPECTED_VERSION": profile["compositor"]["version"],
                "CUA_HYPRLAND_TEST_OPERATOR_KEY": "",
                "CMAKE_HOME_DIRECTORY": str((Path(build).parent / "cua-source/libs/cua-driver/hyprland-plugin").resolve()),
                "HYPRLAND_VERSION": profile["compositor"]["version"],
                "PKG_CONFIG_EXECUTABLE": "/usr/bin/pkgconf",
                "PKG_CONFIG_ARGN": "", "PKG_CONFIG_USE_CMAKE_PREFIX_PATH": "OFF"}
    for key, value in expected.items():
        require(cache.get(key) == value, f"CMake configuration mismatch: {key}")
    stage_headers = str(Path(profile["compositor"]["staged_root"]) / "usr/include")
    paths = cache.get("HYPRLAND_INCLUDE_DIRS", "").split(";")
    require(stage_headers in paths and stage_headers + "/hyprland/src" in paths,
            "effective Hyprland include directories do not select candidate headers")
    pc_env = {**os.environ, "PKG_CONFIG_PATH": str(kit),
              "PKG_CONFIG_ALLOW_SYSTEM_CFLAGS": "1"}
    cflags = subprocess.check_output(["/usr/bin/pkgconf", "--cflags-only-I", "hyprland"],
                                     env=pc_env, text=True).strip().split()
    require(set(paths) == {flag.removeprefix("-I") for flag in cflags if flag.startswith("-I")},
            "CMake include paths differ from reviewed pkgconf selection")
    for key, value in cache.items():
        require(not value or key not in ("CMAKE_PREFIX_PATH", "CMAKE_MODULE_PATH",
                "CMAKE_TOOLCHAIN_FILE", "CMAKE_SYSROOT", "CMAKE_FIND_ROOT_PATH",
                "CMAKE_CXX_COMPILER_LAUNCHER", "CMAKE_CXX_COMPILER_EXTERNAL_TOOLCHAIN",
                "CMAKE_PROJECT_INCLUDE", "CMAKE_PROJECT_INCLUDE_BEFORE",
                "CMAKE_PROJECT_TOP_LEVEL_INCLUDES"),
                f"CMake routing override refused: {key}")
        if key.startswith(("CMAKE_CXX_FLAGS", "CMAKE_EXE_LINKER_FLAGS",
                           "CMAKE_MODULE_LINKER_FLAGS", "CMAKE_SHARED_LINKER_FLAGS")):
            for flag in shlex.split(value):
                parts = flag[4:].split(",") if flag.startswith(("-Wp,", "-Wl,")) else [flag]
                require(all(not part.startswith(("@", *ROUTING_FLAGS)) for part in parts),
                        f"CMake compiler/linker flag override refused: {key}")


def verify_rust_environment(source_root, profile):
    for name, value in os.environ.items():
        routed = name.startswith(("RUSTFLAGS", "RUSTC_", "CARGO_")) or name in (
            "RUSTDOC", "RUSTDOCFLAGS", "CARGO")
        if name in ("CARGO_HOME", "CARGO_TARGET_DIR"):
            continue
        require(not value or not routed, f"Rust compiler/source routing refused: {name}")
    require(os.environ.get("RUSTC", "") == profile["driver_toolchain"]["rustc_path"] and
            rust_toolchain() == profile["driver_toolchain"],
            "Rust compiler/Cargo executables differ from reviewed package profile")
    home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))).resolve()
    root = Path(source_root).resolve()
    for parent in (home, *list(root.parents)):
        for name in ("config", "config.toml"):
            require(not (parent / ".cargo" / name).exists() and
                    not (parent / name).exists() if parent == home else
                    not (parent / ".cargo" / name).exists(),
                    f"unreviewed inherited Cargo configuration: {parent}")


def record_build(kit, module, output, extracted, build):
    profile = verify_builder(kit, extracted, candidate_installed=True)
    verify_cmake(build, kit, profile)
    module = regular(module)
    require(not output.exists(), "build provenance must not be overwritten")
    require(runtime_path(module) == Path(profile["compiler"]["runtime"]) and
            digest(runtime_path(module)) == profile["compiler"]["runtime_sha256"],
            "plugin C++ runtime differs from candidate compositor")
    require("GCC: (GNU) " + profile["compiler"]["gcc_version"] in
            command("readelf", "-p", ".comment", str(module)),
            "plugin compiler comment mismatch")
    provenance = {"schema": 1, "module_sha256": digest(module),
                  "profile_sha256": digest(kit / "PROFILE.json"),
                  "source_archive_sha256": profile["source"]["archive_sha256"],
                  "compositor_sha256": profile["compositor"]["binary_sha256"],
                  "compositor_abi": profile["compositor"]["abi_hash"],
                  "compiler_sha256": profile["compiler"]["sha256"],
                  "compiler_runtime_sha256": profile["compiler"]["runtime_sha256"],
                  "plugin_options": PLUGIN_OPTIONS}
    output.write_bytes(encoded(provenance))
    return provenance

def record_driver(kit, driver, output, extracted):
    profile = verify_builder(kit, extracted)
    verify_rust_environment(Path(extracted) / "libs/cua-driver/rust", profile)
    driver = regular(driver)
    require(not output.exists(), "Driver provenance must not be overwritten")
    require(profile["source"]["driver_version"] in command(str(driver), "--version"),
            "Driver executable does not report pinned component version")
    require(profile["driver_toolchain"] == rust_toolchain(),
            "Driver Rust toolchain changed")
    native_owners = dependency_owners(driver, profile["stock"]["driver_dependencies"])
    provenance = {"schema": 1, "binary_sha256": digest(driver),
                  "profile_sha256": digest(kit / "PROFILE.json"),
                  "source_archive_sha256": profile["source"]["archive_sha256"],
                  "source_base_revision": profile["source"]["base_revision"],
                  "package_version": profile["source"]["package_version"],
                  "rustc": profile["driver_toolchain"]["rustc"],
                  "cargo": profile["driver_toolchain"]["cargo"],
                  "rustup_toolchain": profile["driver_toolchain"]["rustup_toolchain"],
                  "rustc_sha256": profile["driver_toolchain"]["rustc_sha256"],
                  "cargo_sha256": profile["driver_toolchain"]["cargo_sha256"],
                  "rustc_path": profile["driver_toolchain"]["rustc_path"],
                  "cargo_path": profile["driver_toolchain"]["cargo_path"],
                  "native_dependency_owners": native_owners}
    output.write_bytes(encoded(provenance))
    return provenance


def verify_consumer(kit, expected_kit_sha, binary, compositor_binary,
                    driver=None, driver_provenance=None):
    kit = safe_dir(kit)
    require(digest(kit / "KIT-PROVENANCE.json") == expected_kit_sha,
            "kit provenance checksum mismatch")
    provenance = json.loads(regular(kit / "KIT-PROVENANCE.json").read_text())
    profile = json.loads(regular(kit / "PROFILE.json").read_text())
    require(digest(kit / "PROFILE.json") == provenance["profile_sha256"], "profile checksum mismatch")
    require(digest(kit / "profile_verify.py") == provenance["verifier_sha256"],
            "verifier checksum mismatch")
    require(profile["packages"] == installed_packages(),
            "installed ABI runtime packages changed")
    require(digest(compositor_binary) == profile["compositor"]["binary_sha256"],
            "installed compositor differs from reviewed candidate")
    info = json.loads(command(str(compositor_binary), "--version-json"))
    require(info["commit"] == profile["compositor"]["revision"] and
            info["abiHash"] == profile["compositor"]["abi_hash"] and
            info["abiHash"] != profile["stock"]["abi_hash"], "compositor ABI mismatch")
    build = json.loads(regular(kit / "BUILD-PROVENANCE.json").read_text())
    require(build["profile_sha256"] == provenance["profile_sha256"] and
            build["source_archive_sha256"] == profile["source"]["archive_sha256"] and
            build["compositor_sha256"] == profile["compositor"]["binary_sha256"] and
            build["compositor_abi"] == profile["compositor"]["abi_hash"] and
            build["plugin_options"] == profile["plugin_options"] and
            build["compiler_sha256"] == profile["compiler"]["sha256"] and
            build["compiler_runtime_sha256"] == profile["compiler"]["runtime_sha256"],
            "build provenance does not match reviewed profile")
    require(digest(binary) == build["module_sha256"], "installed module bytes changed")
    require(runtime_path(binary) == Path(profile["compiler"]["runtime"]) and
            digest(runtime_path(binary)) == profile["compiler"]["runtime_sha256"],
            "module C++ runtime differs from candidate")
    if driver is not None or driver_provenance is not None:
        require(driver is not None and driver_provenance is not None,
                "Driver binary and provenance must be supplied together")
        driver_build = json.loads(regular(driver_provenance).read_text())
        require(driver_build["schema"] == 1 and
                driver_build["profile_sha256"] == provenance["profile_sha256"] and
                driver_build["source_archive_sha256"] == profile["source"]["archive_sha256"] and
                driver_build["source_base_revision"] == profile["source"]["base_revision"] and
                driver_build["package_version"] == profile["source"]["package_version"] and
                driver_build["rustc"] == profile["driver_toolchain"]["rustc"] and
                driver_build["cargo"] == profile["driver_toolchain"]["cargo"] and
                driver_build["rustup_toolchain"] == profile["driver_toolchain"]["rustup_toolchain"] and
                driver_build["rustc_sha256"] == profile["driver_toolchain"]["rustc_sha256"] and
                driver_build["cargo_sha256"] == profile["driver_toolchain"]["cargo_sha256"] and
                driver_build["rustc_path"] == profile["driver_toolchain"]["rustc_path"] and
                driver_build["cargo_path"] == profile["driver_toolchain"]["cargo_path"] and
                driver_build["native_dependency_owners"] ==
                dependency_owners(driver, profile["stock"]["driver_dependencies"]) and
                digest(regular(driver)) == driver_build["binary_sha256"] and
                profile["source"]["driver_version"] in command(str(driver), "--version"),
                "candidate Driver binary/provenance does not match paired source/profile")
    return profile


def render(template, replacements):
    for key, value in replacements.items():
        template = template.replace("@" + key + "@", value)
    require(not re.search(r"@[A-Z_]+@", template), "unresolved package recipe placeholder")
    return template


def prepare(args):
    repo = safe_dir(args.repo.resolve())
    compositor = safe_dir(args.compositor.resolve())
    stage = safe_dir(args.stage.resolve())
    output = Path(args.output).absolute()
    require(not output.exists(), "candidate kit output already exists")
    files, source = manifest_source(repo)
    archive_bytes = archive(files)
    stage_bytes, inventory = stage_archive(stage)
    profile = candidate_profile(compositor, stage, source, archive_bytes, stage_bytes, inventory)
    profile_bytes = encoded(profile)
    source_bytes = encoded(source)
    script_bytes = regular(__file__).read_bytes()
    pc_bytes = plugin_pkgconfig(stage).encode()
    metadata = {"schema": 1, "profile_sha256": sha(profile_bytes),
                "source_manifest_sha256": sha(source_bytes),
                "source_archive_sha256": sha(archive_bytes),
                "verifier_sha256": sha(script_bytes),
                "pkgconfig_sha256": sha(pc_bytes),
                "compositor_revision": profile["compositor"]["revision"]}
    metadata_bytes = encoded(metadata)
    abi_depends = " ".join(f"'{name}={version}'" for name, version in sorted(profile["packages"].items()))
    compositor_depends = " ".join(f"'{item}'" for item in
                                  (profile["stock"]["compositor_dependencies"] +
                                   profile["compositor"]["extra_dependencies"]))
    driver_depends = " ".join(f"'{item}'" for item in profile["stock"]["driver_dependencies"])
    plugin_libei = [item for item in profile["compositor"]["extra_dependencies"]
                    if item.startswith("libei=")]
    require(len(plugin_libei) == 1, "plugin libeis provider missing from candidate native profile")
    recipes = {
        "plugin": repo / "libs/cua-driver/hyprland-plugin/packaging/arch/PKGBUILD",
        "driver": repo / "downstream/omarchy/driver/PKGBUILD",
    }
    common = {"DRIVER_VERSION": profile["source"]["package_version"],
              "SOURCE_SHA256": sha(archive_bytes), "SOURCE_MANIFEST_SHA256": sha(source_bytes),
              "PROFILE_SHA256": sha(profile_bytes), "KIT_SHA256": sha(metadata_bytes),
              "VERIFIER_SHA256": sha(script_bytes)}
    for kind, recipe in recipes.items():
        kit = output / kind
        kit.mkdir(parents=True, mode=0o700)
        for name, data in {"cua-source.tar.gz": archive_bytes,
                           "SOURCE-PROVENANCE.json": source_bytes, "PROFILE.json": profile_bytes,
                           "KIT-PROVENANCE.json": metadata_bytes, "profile_verify.py": script_bytes}.items():
            (kit / name).write_bytes(data)
        if kind == "plugin":
            (kit / "hyprland.pc").write_bytes(pc_bytes)
            replacements = {**common, "COMPOSITOR_PACKAGE_VERSION": profile["compositor"]["package_version"],
                            "ABI_DEPENDS": abi_depends, "PLUGIN_LIBEI_DEPEND": plugin_libei[0],
                            "PKGCONFIG_SHA256": sha(pc_bytes)}
        else:
            replacements = {**common, "DRIVER_DEPENDS": driver_depends,
                            "GLIBC_VERSION": profile["packages"]["glibc"],
                            "LIBGCC_VERSION": profile["packages"]["libgcc"],
                            "RUSTC_VERSION": profile["driver_toolchain"]["rustc"],
                            "CARGO_VERSION": profile["driver_toolchain"]["cargo"],
                            "RUSTUP_TOOLCHAIN": profile["driver_toolchain"]["rustup_toolchain"],
                            "RUSTC_PATH": profile["driver_toolchain"]["rustc_path"],
                            "CARGO_PATH": profile["driver_toolchain"]["cargo_path"]}
        (kit / "PKGBUILD").write_text(render(regular(recipe).read_text(), replacements))
    compositor_kit = output / "compositor"
    compositor_kit.mkdir(parents=True, mode=0o700)
    license_bytes = regular(compositor / "LICENSE").read_bytes()
    for name, data in {"candidate-compositor.tar.gz": stage_bytes, "PROFILE.json": profile_bytes,
                       "profile_verify.py": script_bytes, "HYPRLAND-LICENSE": license_bytes}.items():
        (compositor_kit / name).write_bytes(data)
    replacements = {"COMPOSITOR_PKGVER": profile["compositor"]["package_version"].removesuffix("-1"),
                    "ABI_DEPENDS": abi_depends, "COMPOSITOR_DEPENDS": compositor_depends,
                    "STAGE_SHA256": sha(stage_bytes),
                    "PROFILE_SHA256": sha(profile_bytes), "VERIFIER_SHA256": sha(script_bytes),
                    "HYPRLAND_LICENSE_SHA256": sha(license_bytes)}
    recipe = repo / "downstream/omarchy/compositor/PKGBUILD"
    (compositor_kit / "PKGBUILD").write_text(render(regular(recipe).read_text(), replacements))
    return profile


def cli():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    prep = sub.add_parser("prepare", help="snapshot exact local sources and staged fork ABI; no build/load")
    for name in ("repo", "compositor", "stage", "output"):
        prep.add_argument("--" + name, type=Path, required=True)
    check = sub.add_parser("verify-build", help="refuse source/header/compiler/ABI drift before package build")
    check.add_argument("--kit", type=Path, required=True)
    check.add_argument("--extracted", type=Path, required=True)
    check.add_argument("--require-installed-candidate", action="store_true")
    check.add_argument("--rust", action="store_true")
    stage = sub.add_parser("verify-stage", help="verify staged compositor package against candidate profile")
    stage.add_argument("--kit", type=Path, required=True)
    stage.add_argument("--extracted", type=Path, required=True)
    record = sub.add_parser("record-build", help="record verified plugin ELF after CMake build")
    record.add_argument("--kit", type=Path, required=True)
    record.add_argument("--module", type=Path, required=True)
    record.add_argument("--output", type=Path, required=True)
    record.add_argument("--extracted", type=Path, required=True)
    record.add_argument("--build", type=Path, required=True)
    driver = sub.add_parser("record-driver", help="record checked Driver binary provenance")
    driver.add_argument("--kit", type=Path, required=True)
    driver.add_argument("--driver", type=Path, required=True)
    driver.add_argument("--output", type=Path, required=True)
    driver.add_argument("--extracted", type=Path, required=True)
    consumer = sub.add_parser("verify-consumer", help="verify installed module and exact compositor bytes")
    consumer.add_argument("--kit", type=Path, required=True)
    consumer.add_argument("--kit-sha256", required=True)
    consumer.add_argument("--consumer", type=Path, required=True)
    consumer.add_argument("--compositor", type=Path, default=Path("/usr/bin/Hyprland"))
    consumer.add_argument("--driver", type=Path)
    consumer.add_argument("--driver-provenance", type=Path)
    args = parser.parse_args()
    try:
        if args.operation == "prepare":
            result = prepare(args)
            print(json.dumps({"kit": str(args.output), "source_sha256": result["source"]["archive_sha256"],
                              "compositor_abi": result["compositor"]["abi_hash"]}))
        elif args.operation == "verify-build":
            result = verify_builder(args.kit, args.extracted, args.require_installed_candidate)
            if args.rust:
                verify_rust_environment(args.extracted / "libs/cua-driver/rust", result)
            print(json.dumps({"compositor_abi": result["compositor"]["abi_hash"],
                              "source_sha256": result["source"]["archive_sha256"]}))
        elif args.operation == "verify-stage":
            result = verify_stage(args.kit, args.extracted)
            print(json.dumps({"compositor_abi": result["compositor"]["abi_hash"],
                              "stage_sha256": result["compositor"]["stage_archive_sha256"]}))
        elif args.operation == "record-build":
            result = record_build(args.kit, args.module, args.output, args.extracted, args.build)
            print(json.dumps({"module_sha256": result["module_sha256"],
                              "compositor_abi": result["compositor_abi"]}))
        elif args.operation == "record-driver":
            result = record_driver(args.kit, args.driver, args.output, args.extracted)
            print(json.dumps({"driver_sha256": result["binary_sha256"],
                              "source_sha256": result["source_archive_sha256"]}))
        else:
            result = verify_consumer(args.kit, args.kit_sha256, args.consumer, args.compositor,
                                     args.driver, args.driver_provenance)
            print(json.dumps({"compositor_abi": result["compositor"]["abi_hash"],
                              "module_sha256": digest(args.consumer)}))
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        parser.exit(1, f"error: {error}\n")


if __name__ == "__main__":
    cli()

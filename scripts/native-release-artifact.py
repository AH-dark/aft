#!/usr/bin/env python3
"""Transfer Cargo release artifacts bound to the same Git SHA, compiler and target."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[1]
ENV_KEYS = ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "MACOSX_DEPLOYMENT_TARGET", "CARGO_INCREMENTAL", "CARGO_PROFILE_RELEASE_LTO", "CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "CARGO_PROFILE_RELEASE_DEBUG", "CARGO_PROFILE_RELEASE_STRIP")


def digest(path: Path) -> str:
    checksum = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def output(*args: str) -> str:
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def manifest_digest() -> str:
    # Hash the committed Cargo.toml, not the checked-out file: Windows runners
    # check out with CRLF line endings, so working-tree bytes differ by platform
    # for the same commit and a Windows build could never match publication on
    # Linux.
    blob = subprocess.check_output(["git", "show", "HEAD:Cargo.toml"], cwd=ROOT)
    return hashlib.sha256(blob).hexdigest()


def facts(target: str) -> dict:
    rust = output("rustc", "--version", "--verbose")
    version = rust.splitlines()[0].split()[1]
    host = next(line.split(": ", 1)[1] for line in rust.splitlines() if line.startswith("host: "))
    cargo = json.loads(output("cargo", "metadata", "--format-version", "1", "--no-deps"))
    package = next(p for p in cargo["packages"] if p["name"] == "agent-file-tools")
    return {"schema": 1, "git_sha": output("git", "rev-parse", "HEAD"), "profile": "release",
            "target": target or host, "rust_version": version, "rust_verbose": rust,
            "package_version": package["version"], "profile_manifest_sha256": manifest_digest(),
            "env": {name: os.environ.get(name, "") for name in ENV_KEYS}}


def release_dir(target: str) -> Path:
    return ROOT / "target" / target / "release" if target else ROOT / "target/release"


def input_clocks() -> dict:
    paths = subprocess.check_output(["git", "ls-files", "-z", "--", "Cargo.toml", "Cargo.lock", "crates", ".cargo"], cwd=ROOT).decode().split("\0")
    clocks = {}
    for name in paths:
        path = ROOT / name
        if name and path.is_file():
            clocks[name] = {"mtime_ns": path.stat().st_mtime_ns, "sha256": digest(path)}
    # Cargo reruns build.rs when Git ref timestamps change, even at the same
    # commit. The library handoff restores these clocks after checking its Git
    # SHA, compiler, target, release profile and tracked source-file hashes.
    git_paths = ["HEAD", "packed-refs", "refs"]
    branch = subprocess.run(["git", "symbolic-ref", "-q", "HEAD"], cwd=ROOT, text=True, capture_output=True)
    if branch.returncode == 0:
        git_paths.append(branch.stdout.strip())
    for name in git_paths:
        path = Path(output("git", "rev-parse", "--git-path", name))
        path = path if path.is_absolute() else ROOT / path
        try:
            relative = path.resolve().relative_to(ROOT.resolve())
        except ValueError:
            continue
        if path.exists():
            clocks[relative.as_posix()] = {"mtime_ns": path.stat().st_mtime_ns, "git_clock": True}
    return clocks


def safe_extract(archive: Path, destination: Path, allowed: tuple[str, ...]) -> None:
    with tarfile.open(archive) as bundle:
        for member in bundle.getmembers():
            name = PurePosixPath(member.name)
            if name.is_absolute() or ".." in name.parts or not (member.isfile() or member.isdir()):
                raise ValueError("release artifact contains an unsafe path or link")
            if not any(member.name == prefix or member.name.startswith(prefix + "/") for prefix in allowed):
                raise ValueError("release artifact contains an unexpected file")
            destination_path = destination / member.name
            if not destination_path.resolve().is_relative_to(destination.resolve()):
                raise ValueError("release artifact destination escaped checkout")
        bundle.extractall(destination)


def validate(expected: dict, actual: dict, compiler: bool = True) -> None:
    keys = ("schema", "git_sha", "profile", "target", "package_version", "profile_manifest_sha256", "env")
    if compiler:
        keys += ("rust_verbose",)
    for name in keys:
        if expected.get(name) != actual.get(name):
            raise ValueError(f"release artifact mismatch: {name}")


def verify_binary(data: dict, executable: Path, dsym: Path | None = None) -> None:
    if data.get("kind") != "binary":
        raise ValueError("native release artifact has the wrong build stage")
    if digest(executable) != data["binary_sha256"]:
        raise ValueError("native release binary checksum mismatch")
    if dsym is not None and digest(dsym) != data.get("dsym_sha256"):
        raise ValueError("native release dSYM checksum mismatch")


def unpack_published(archive: Path, target: str, executable: Path, dsym: Path | None) -> None:
    # Publication runs on Linux even for macOS and Windows artifacts. It checks
    # release identity and bytes without requiring the producer's Rust compiler.
    with tempfile.TemporaryDirectory(prefix="aft-release-") as temp:
        root = Path(temp)
        safe_extract(archive, root, ("target", "native-release-manifest.json", "native-release-dsym.zip"))
        data = json.loads((root / "native-release-manifest.json").read_text())
        expected = {"schema": 1, "git_sha": output("git", "rev-parse", "HEAD"),
                    "profile": "release", "target": target,
                    "profile_manifest_sha256": manifest_digest()}
        for name, value in expected.items():
            if data.get(name) != value:
                raise ValueError(f"release artifact mismatch: {name}")
        filename = "aft.exe" if target.endswith("windows-msvc") else "aft"
        source = root / "target" / target / "release" / filename
        if not source.exists():
            source = root / "target/release" / filename
        debug = root / "native-release-dsym.zip" if data.get("dsym_sha256") or dsym else None
        verify_binary(data, source, debug)
        executable.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, executable)
        if dsym is not None:
            assert debug is not None
            dsym.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(debug, dsym)
        verify_binary(data, executable, dsym)
        print(f"Verified published binary SHA256 {data['binary_sha256']} from {data['git_sha']} / {target}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("pack-library", "unpack-library", "pack-binary", "unpack-binary", "verify-binary", "unpack-published"))
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--target", default="")
    parser.add_argument("--dsym", type=Path)
    parser.add_argument("--binary", type=Path)
    args = parser.parse_args()
    if args.mode == "unpack-published":
        if not args.target or args.binary is None:
            parser.error("unpack-published requires --target and --binary")
        unpack_published(args.archive, args.target, args.binary, args.dsym)
        return
    if args.mode == "verify-binary":
        with tarfile.open(args.archive) as bundle:
            manifest = bundle.extractfile("native-release-manifest.json")
            if manifest is None:
                raise ValueError("native release binary manifest is missing")
            data = json.load(manifest)
        validate(facts(args.target), data)
        executable = args.binary or release_dir(args.target) / ("aft.exe" if data["target"].endswith("windows-msvc") else "aft")
        verify_binary(data, executable)
        print(f"Verified tested binary SHA256 {data['binary_sha256']} from {data['git_sha']}")
        return
    directory = release_dir(args.target)
    manifest_path = ROOT / "native-release-manifest.json"
    if args.mode.startswith("pack"):
        data = facts(args.target)
        data["kind"] = "library" if args.mode == "pack-library" else "binary"
        executable = directory / ("aft.exe" if data["target"].endswith("windows-msvc") else "aft")
        if data["kind"] == "library":
            libraries = sorted(path for path in directory.rglob("*.rlib")
                               if path.stem in ("aft", "libaft") or path.stem.startswith(("aft-", "libaft-")))
            if not libraries:
                raise ValueError("native release library codegen output is missing")
            data["library_outputs"] = {path.relative_to(ROOT).as_posix(): digest(path) for path in libraries}
            data["input_clocks"] = input_clocks()
        else:
            data["binary_sha256"] = digest(executable)
            if args.dsym:
                data["dsym_sha256"] = digest(args.dsym)
        manifest_path.write_text(json.dumps(data, indent=2) + "\n")
        args.archive.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(args.archive, "w", dereference=True) as bundle:
            bundle.add(manifest_path, arcname="native-release-manifest.json")
            if data["kind"] == "library":
                bundle.add(directory, arcname=directory.relative_to(ROOT).as_posix())
            else:
                bundle.add(executable, arcname=executable.relative_to(ROOT).as_posix())
                if args.dsym:
                    bundle.add(args.dsym, arcname="native-release-dsym.zip")
        if os.environ.get("GITHUB_OUTPUT"):
            with open(os.environ["GITHUB_OUTPUT"], "a") as gh:
                gh.write(f"rust_version={data['rust_version']}\n")
        print(f"Native release {data['kind']} artifact packed for {data['git_sha']} / {data['target']} / Rust {data['rust_version']}")
    else:
        safe_extract(args.archive, ROOT, ("target", "native-release-manifest.json", "native-release-dsym.zip"))
        data = json.loads(manifest_path.read_text())
        validate(facts(args.target), data)
        expected_kind = "library" if args.mode == "unpack-library" else "binary"
        if data.get("kind") != expected_kind:
            raise ValueError("native release artifact has the wrong build stage")
        if expected_kind == "library":
            if not data.get("library_outputs"):
                raise ValueError("native release library artifact has no compiled output")
            for name, checksum in data["library_outputs"].items():
                path = ROOT / name
                if not path.resolve().is_relative_to(ROOT.resolve()) or digest(path) != checksum:
                    raise ValueError("native release library output checksum mismatch")
            for name, clock in data["input_clocks"].items():
                path = ROOT / name
                if not path.resolve().is_relative_to(ROOT.resolve()):
                    raise ValueError("release input clock escaped checkout")
                if not path.exists():
                    if clock.get("git_clock"):
                        continue
                    raise ValueError(f"release source input missing: {name}")
                if not clock.get("git_clock") and digest(path) != clock["sha256"]:
                    raise ValueError(f"release source input mismatch: {name}")
                os.utime(path, ns=(path.stat().st_atime_ns, clock["mtime_ns"]))
        else:
            executable = directory / ("aft.exe" if data["target"].endswith("windows-msvc") else "aft")
            verify_binary(data, executable, ROOT / "native-release-dsym.zip" if data.get("dsym_sha256") else None)
        print(f"Verified native release {expected_kind} artifact from {data['git_sha']}")


if __name__ == "__main__":
    main()

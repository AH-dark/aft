#!/usr/bin/env python3
"""Reject native release handoffs with different code, profile, compiler or paths."""

import importlib.util
from contextlib import contextmanager
import hashlib
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("native_release", Path(__file__).resolve().parents[1] / "native-release-artifact.py")
assert SPEC is not None and SPEC.loader is not None
native = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(native)


def facts():
    return {"schema": 1, "git_sha": "a" * 40, "profile": "release", "target": "aarch64-apple-darwin",
            "package_version": "0.59.0", "profile_manifest_sha256": "b" * 64,
             "env": {"RUSTFLAGS": ""}, "rust_version": "1.99.0",
             "rust_verbose": "rustc 1.99.0\nhost: aarch64-apple-darwin"}


@contextmanager
def handoff(kind="library"):
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        source = root / "Cargo.toml"
        source.write_bytes(b"tracked source before transfer")
        clocks = {"Cargo.toml": {"mtime_ns": source.stat().st_mtime_ns,
                                "sha256": hashlib.sha256(source.read_bytes()).hexdigest()}}
        directory = root / "target/release"
        directory.mkdir(parents=True)
        product = directory / ("libaft.rlib" if kind == "library" else "aft")
        product.write_bytes(b"compiled native release output")
        archive = root / "handoff.tar"
        with patch.object(native, "ROOT", root), patch.object(native, "facts", return_value=facts()), \
                patch.object(native, "input_clocks", return_value=clocks):
            with patch("sys.argv", ["native-release-artifact.py", f"pack-{kind}", "--archive", str(archive)]):
                native.main()
            yield root, archive, product, clocks


def unpack(archive, kind="library"):
    with patch("sys.argv", ["native-release-artifact.py", f"unpack-{kind}", "--archive", str(archive)]):
        native.main()


class NativeReleaseArtifactTests(unittest.TestCase):
    def test_library_handoff_restores_compiled_output_and_verified_source_clock(self):
        with handoff() as (root, archive, product, clocks):
            product.write_bytes(b"stale compiled output in consumer")
            source = root / "Cargo.toml"
            os.utime(source, ns=(source.stat().st_atime_ns, clocks["Cargo.toml"]["mtime_ns"] + 1_000_000_000))
            unpack(archive)
            self.assertEqual(product.read_bytes(), b"compiled native release output")
            self.assertEqual(source.stat().st_mtime_ns, clocks["Cargo.toml"]["mtime_ns"])

    def test_library_handoff_rejects_changed_tracked_source(self):
        with handoff() as (root, archive, _, _):
            (root / "Cargo.toml").write_bytes(b"different source at same Git SHA")
            with self.assertRaisesRegex(ValueError, "release source input mismatch"):
                unpack(archive)

    def test_library_handoff_rejects_missing_tracked_source(self):
        with handoff() as (root, archive, _, _):
            (root / "Cargo.toml").unlink()
            with self.assertRaisesRegex(ValueError, "release source input missing"):
                unpack(archive)

    def test_binary_handoff_rejects_changed_shipped_bytes(self):
        with handoff("binary") as (root, archive, product, _):
            product.write_bytes(b"binary that never passed E2E")
            with tarfile.open(archive, "w") as bundle:
                bundle.add(root / "native-release-manifest.json", arcname="native-release-manifest.json")
                bundle.add(product, arcname="target/release/aft")
            with self.assertRaisesRegex(ValueError, "binary checksum mismatch"):
                unpack(archive, "binary")

    def test_exact_native_release_metadata_is_accepted(self):
        native.validate(facts(), facts())

    def test_changed_sha_profile_target_version_flags_or_compiler_is_rejected(self):
        for key in ("git_sha", "profile", "target", "package_version", "profile_manifest_sha256", "env", "rust_verbose"):
            with self.subTest(key=key):
                changed = {**facts(), key: "changed"}
                with self.assertRaisesRegex(ValueError, key):
                    native.validate(facts(), changed)

    def test_path_traversal_absolute_paths_and_links_are_rejected(self):
        for name, kind in (("../escape", tarfile.REGTYPE), ("/absolute", tarfile.REGTYPE),
                           ("target/release/link", tarfile.SYMTYPE), ("not-a-release-file", tarfile.REGTYPE)):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temp:
                archive = Path(temp) / "bundle.tar"
                with tarfile.open(archive, "w") as bundle:
                    member = tarfile.TarInfo(name)
                    member.type = kind
                    member.linkname = "../../escape" if kind == tarfile.SYMTYPE else ""
                    bundle.addfile(member)
                with self.assertRaises(ValueError):
                    native.safe_extract(archive, Path(temp) / "checkout", ("target", "native-release-manifest.json"))

    def test_existing_checkout_symlink_cannot_redirect_release_extraction(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            checkout = root / "checkout"
            checkout.mkdir()
            outside = root / "outside"
            outside.mkdir()
            (checkout / "target").symlink_to(outside, target_is_directory=True)
            archive = root / "bundle.tar"
            with tarfile.open(archive, "w") as bundle:
                bundle.addfile(tarfile.TarInfo("target/release/aft"))
            with self.assertRaisesRegex(ValueError, "escaped"):
                native.safe_extract(archive, checkout, ("target",))

    def test_regular_native_release_files_extract_without_path_rewriting(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = root / "bundle.tar"
            with tarfile.open(archive, "w") as bundle:
                bundle.addfile(tarfile.TarInfo("target/release/aft"))
            native.safe_extract(archive, root / "checkout", ("target",))
            self.assertTrue((root / "checkout/target/release/aft").is_file())


if __name__ == "__main__":
    unittest.main()

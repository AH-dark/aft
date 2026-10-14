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
def handoff(kind="library", target="aarch64-apple-darwin", explicit=False):
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        source = root / "Cargo.toml"
        source.write_bytes(b"tracked source before transfer")
        clocks = {"Cargo.toml": {"mtime_ns": source.stat().st_mtime_ns,
                                "sha256": hashlib.sha256(source.read_bytes()).hexdigest()}}
        directory = root / "target" / target / "release" if explicit else root / "target/release"
        directory.mkdir(parents=True)
        filename = "aft.exe" if target.endswith("windows-msvc") else "aft"
        product = directory / ("libaft.rlib" if kind == "library" else filename)
        product.write_bytes(b"compiled native release output")
        archive = root / "handoff.tar"
        expected = {**facts(), "target": target, "profile_manifest_sha256": native.digest(source)}
        with patch.object(native, "ROOT", root), patch.object(native, "facts", return_value=expected), \
                patch.object(native, "input_clocks", return_value=clocks):
            argv = ["native-release-artifact.py", f"pack-{kind}", "--archive", str(archive)]
            if explicit:
                argv += ["--target", target]
            with patch("sys.argv", argv):
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

    def test_post_e2e_verification_rejects_mutation_without_restoring_binary(self):
        with handoff("binary") as (_, archive, product, _):
            product.write_bytes(b"changed while E2E ran")
            with patch("sys.argv", ["native-release-artifact.py", "verify-binary", "--archive", str(archive)]):
                with self.assertRaisesRegex(ValueError, "binary checksum mismatch"):
                    native.main()
            self.assertEqual(product.read_bytes(), b"changed while E2E ran")

    def test_published_binary_checksum_equals_tested_bytes_without_rust(self):
        for target, explicit in (("aarch64-apple-darwin", False), ("x86_64-pc-windows-msvc", True),
                                 ("x86_64-unknown-linux-gnu", True)):
            with self.subTest(target=target), handoff("binary", target, explicit) as (root, archive, product, _):
                tested = product.read_bytes()
                destination = root / "npm/bin" / product.name
                product.unlink()
                with patch.object(native, "output", return_value="a" * 40) as output, \
                        patch.object(native, "facts", side_effect=AssertionError("publication must not run Rust")):
                    native.unpack_published(archive, target, destination, None)
                output.assert_called_once_with("git", "rev-parse", "HEAD")
                self.assertEqual(destination.read_bytes(), tested)
                self.assertEqual(hashlib.sha256(destination.read_bytes()).hexdigest(), hashlib.sha256(tested).hexdigest())

    def test_publication_rejects_a_different_sha_or_target(self):
        with handoff("binary") as (root, archive, _, _):
            destination = root / "release-assets/aft"
            for sha, target, error in (("c" * 40, "aarch64-apple-darwin", "git_sha"),
                                       ("a" * 40, "x86_64-unknown-linux-gnu", "target")):
                with self.subTest(error=error), patch.object(native, "output", return_value=sha):
                    with self.assertRaisesRegex(ValueError, error):
                        native.unpack_published(archive, target, destination, None)
                    self.assertFalse(destination.exists())

    def test_publication_rejects_tampered_tested_bundle(self):
        with handoff("binary") as (root, archive, product, _):
            product.write_bytes(b"rebuild that never passed E2E")
            with tarfile.open(archive, "w") as bundle:
                bundle.add(root / "native-release-manifest.json", arcname="native-release-manifest.json")
                bundle.add(product, arcname="target/release/aft")
            destination = root / "npm/bin/aft"
            with patch.object(native, "output", return_value="a" * 40):
                with self.assertRaisesRegex(ValueError, "binary checksum mismatch"):
                    native.unpack_published(archive, "aarch64-apple-darwin", destination, None)
            self.assertFalse(destination.exists())

    def test_publication_preserves_verified_dsym_bytes(self):
        with handoff("binary") as (root, archive, _, _):
            debug = root / "producer-dsym.zip"
            debug.write_bytes(b"debug symbols for the tested executable")
            with patch("sys.argv", ["native-release-artifact.py", "pack-binary", "--archive", str(archive), "--dsym", str(debug)]):
                native.main()
            destination = root / "release-assets/aft-dsym.zip"
            with patch.object(native, "output", return_value="a" * 40):
                native.unpack_published(archive, "aarch64-apple-darwin", root / "release-assets/aft", destination)
            self.assertEqual(destination.read_bytes(), debug.read_bytes())

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

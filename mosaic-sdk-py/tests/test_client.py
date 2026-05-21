"""Integration tests against a live mosaic-serve process."""

from __future__ import annotations

import asyncio
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

# Allow running from the package root without installing.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from mosaic import LiveSession, MosaicClient, MosaicError  # noqa: E402

MOS = os.environ.get("MOS_BIN", "/home/user/TestRepo/target/debug/mos")
SERVE = os.environ.get("MOS_SERVE_BIN", "/home/user/TestRepo/target/debug/mosaic-serve")


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def fresh_repo() -> str:
    repo = tempfile.mkdtemp(prefix="mosaic-py-")
    subprocess.run([MOS, "init"], cwd=repo, check=True, capture_output=True)
    subprocess.run(
        [MOS, "id", "setup", "--email", "py@example.com", "--name", "Py"],
        cwd=repo,
        check=True,
        capture_output=True,
    )
    return repo


def start_server(repo: str) -> tuple[str, subprocess.Popen]:
    port = free_port()
    proc = subprocess.Popen(
        [SERVE, "--repo", repo, "--bind", f"127.0.0.1:{port}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    url = f"http://127.0.0.1:{port}"
    client = MosaicClient(url)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            client.health()
            return url, proc
        except Exception:
            time.sleep(0.05)
    proc.kill()
    raise RuntimeError("server never became healthy")


class TestMosaicClient(unittest.TestCase):
    def test_health_empty_repo(self) -> None:
        repo = fresh_repo()
        url, proc = start_server(repo)
        try:
            c = MosaicClient(url)
            h = c.health()
            self.assertTrue(h.ok)
            self.assertEqual(h.changes, 0)
            self.assertEqual(h.branches, 0)
        finally:
            proc.kill()
            shutil.rmtree(repo, ignore_errors=True)

    def test_commit_then_list_changes(self) -> None:
        repo = fresh_repo()
        (Path(repo) / "hello.txt").write_text("hello\n")
        subprocess.run(
            [MOS, "commit", "-i", "first via py", "-f", "hello.txt"],
            cwd=repo,
            check=True,
            capture_output=True,
        )
        url, proc = start_server(repo)
        try:
            c = MosaicClient(url)
            branches = c.branches()
            self.assertEqual(len(branches), 1)
            self.assertEqual(branches[0].name, "main")

            changes = c.changes()
            self.assertEqual(len(changes), 1)
            self.assertEqual(changes[0].intent, "first via py")

            detail = c.change(changes[0].id)
            self.assertEqual(len(detail.files), 1)
            self.assertEqual(detail.files[0]["path"], "hello.txt")
        finally:
            proc.kill()
            shutil.rmtree(repo, ignore_errors=True)

    def test_missing_branch_raises_404(self) -> None:
        repo = fresh_repo()
        url, proc = start_server(repo)
        try:
            c = MosaicClient(url)
            with self.assertRaises(MosaicError) as ctx:
                c.branch("nope")
            self.assertEqual(ctx.exception.status, 404)
        finally:
            proc.kill()
            shutil.rmtree(repo, ignore_errors=True)

    def test_push_then_pull_round_trip(self) -> None:
        source = fresh_repo()
        (Path(source) / "a.txt").write_text("alpha\n")
        subprocess.run(
            [MOS, "commit", "-i", "alpha via py", "-f", "a.txt"],
            cwd=source,
            check=True,
            capture_output=True,
        )
        bundle_path = Path(source) / "out.bundle"
        subprocess.run(
            [MOS, "bundle", "create", "-o", str(bundle_path), "--branch", "main"],
            cwd=source,
            check=True,
            capture_output=True,
        )
        bundle_bytes = bundle_path.read_bytes()

        server_repo = fresh_repo()
        url, proc = start_server(server_repo)
        try:
            c = MosaicClient(url)
            report = c.push_bundle(bundle_bytes)
            self.assertEqual(len(report.applied), 1)
            self.assertEqual(len(report.skipped), 0)

            health = c.health()
            self.assertEqual(health.changes, 1)
            self.assertEqual(health.branches, 1)

            pulled = c.pull_bundle("main", [])
            self.assertGreater(len(pulled), 0)
            self.assertEqual(pulled[:8], b"MOSAICB1")
        finally:
            proc.kill()
            shutil.rmtree(source, ignore_errors=True)
            shutil.rmtree(server_repo, ignore_errors=True)

    def test_diff_endpoint(self) -> None:
        repo = fresh_repo()
        (Path(repo) / "f.txt").write_text("alpha\nbeta\n")
        subprocess.run([MOS, "commit", "-i", "init", "-f", "f.txt"], cwd=repo, check=True, capture_output=True)
        (Path(repo) / "f.txt").write_text("alpha\nBETA\ngamma\n")
        subprocess.run([MOS, "commit", "-i", "edit", "-f", "f.txt"], cwd=repo, check=True, capture_output=True)

        url, proc = start_server(repo)
        try:
            c = MosaicClient(url)
            changes = c.changes()
            diff = c.change_diff(changes[1].id)
            self.assertEqual(len(diff), 1)
            self.assertEqual(diff[0].path, "f.txt")
            self.assertEqual(diff[0].status, "modified")
            tags = [h["tag"] for h in diff[0].hunks]
            self.assertIn("insert", tags)
            self.assertIn("delete", tags)
        finally:
            proc.kill()
            shutil.rmtree(repo, ignore_errors=True)


class TestLiveSession(unittest.TestCase):
    def test_two_peers_exchange_updates(self) -> None:
        repo = fresh_repo()
        url, proc = start_server(repo)
        try:
            ws_base = url.replace("http://", "ws://")

            async def scenario() -> bytes:
                alice = await LiveSession.connect(f"{ws_base}/ws/doc/payments.py")
                await asyncio.sleep(0.05)
                bob = await LiveSession.connect(f"{ws_base}/ws/doc/payments.py")
                await asyncio.sleep(0.05)
                await alice.send(b"hello-from-alice")
                got = await asyncio.wait_for(bob.recv(), timeout=2)
                await alice.close()
                await bob.close()
                return got

            received = asyncio.run(scenario())
            self.assertEqual(received, b"hello-from-alice")
        finally:
            proc.kill()
            shutil.rmtree(repo, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()

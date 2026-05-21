"""HTTP client for Mosaic VCS — stdlib only, no extra dependencies."""

from __future__ import annotations

import json
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any, Iterable


class MosaicError(Exception):
    """Raised when a Mosaic API call fails."""

    def __init__(self, message: str, status: int | None = None):
        super().__init__(message)
        self.status = status


@dataclass(frozen=True)
class HealthResponse:
    ok: bool
    repo: str
    changes: int
    branches: int


@dataclass(frozen=True)
class BranchSummary:
    name: str
    tips: tuple[str, ...]


@dataclass(frozen=True)
class ChangeSummary:
    id: str
    author: str
    intent: str | None
    timestamp: int
    deps: tuple[str, ...]
    file_count: int


@dataclass(frozen=True)
class ChangeDetail:
    id: str
    author: str
    intent: str | None
    timestamp: int
    deps: tuple[str, ...]
    files: tuple[dict, ...]


@dataclass(frozen=True)
class FileDiff:
    path: str
    kind: str
    status: str
    hunks: tuple[dict, ...]


@dataclass(frozen=True)
class ApplyReport:
    applied: tuple[str, ...]
    skipped: tuple[str, ...]


class MosaicClient:
    """Thin HTTP client over the mosaic-serve REST API."""

    def __init__(self, base_url: str, *, timeout: float = 10.0):
        self.base = base_url.rstrip("/")
        self.timeout = timeout

    # ---- Plain getters --------------------------------------------------

    def health(self) -> HealthResponse:
        data = self._get_json("/api/v1/health")
        return HealthResponse(**data)

    def branches(self) -> list[BranchSummary]:
        data = self._get_json("/api/v1/branches")
        return [BranchSummary(name=b["name"], tips=tuple(b["tips"])) for b in data]

    def branch(self, name: str) -> BranchSummary:
        data = self._get_json(f"/api/v1/branches/{urllib.parse.quote(name, safe='')}")
        return BranchSummary(name=data["name"], tips=tuple(data["tips"]))

    def changes(self) -> list[ChangeSummary]:
        data = self._get_json("/api/v1/changes")
        return [
            ChangeSummary(
                id=c["id"],
                author=c["author"],
                intent=c.get("intent"),
                timestamp=c["timestamp"],
                deps=tuple(c["deps"]),
                file_count=c["file_count"],
            )
            for c in data
        ]

    def change(self, change_id: str) -> ChangeDetail:
        data = self._get_json(f"/api/v1/changes/{urllib.parse.quote(change_id, safe='')}")
        return ChangeDetail(
            id=data["id"],
            author=data["author"],
            intent=data.get("intent"),
            timestamp=data["timestamp"],
            deps=tuple(data["deps"]),
            files=tuple(data["files"]),
        )

    def change_diff(self, change_id: str) -> list[FileDiff]:
        data = self._get_json(
            f"/api/v1/changes/{urllib.parse.quote(change_id, safe='')}/diff"
        )
        return [
            FileDiff(
                path=d["path"],
                kind=d["kind"],
                status=d["status"],
                hunks=tuple(d["hunks"]),
            )
            for d in data
        ]

    # ---- Bundle endpoints -----------------------------------------------

    def pull_bundle(self, branch: str, have: Iterable[str] = ()) -> bytes:
        query = urllib.parse.urlencode({"branch": branch, "have": ",".join(have)})
        req = urllib.request.Request(f"{self.base}/api/v1/missing?{query}")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                return resp.read()
        except urllib.error.HTTPError as e:
            raise MosaicError(
                f"pull_bundle failed: {e.code} {e.reason}", status=e.code
            ) from None

    def push_bundle(self, bundle: bytes) -> ApplyReport:
        req = urllib.request.Request(
            f"{self.base}/api/v1/bundle",
            data=bundle,
            method="POST",
            headers={"content-type": "application/octet-stream"},
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                body = json.loads(resp.read())
        except urllib.error.HTTPError as e:
            payload = e.read().decode("utf-8", errors="replace")
            try:
                msg = json.loads(payload).get("error", payload)
            except Exception:
                msg = payload
            raise MosaicError(f"push_bundle: {msg}", status=e.code) from None
        return ApplyReport(
            applied=tuple(body["applied"]),
            skipped=tuple(body["skipped"]),
        )

    # ---- Conveniences ---------------------------------------------------

    def wait_for_change(
        self,
        change_id: str,
        *,
        timeout_s: float = 30.0,
        poll_s: float = 0.5,
    ) -> None:
        """Poll until ``change_id`` shows up on the server, else raise."""
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if any(c.id == change_id for c in self.changes()):
                return
            time.sleep(poll_s)
        raise MosaicError(
            f"timeout waiting for change {change_id[:12]} on {self.base}"
        )

    # ---- Internals ------------------------------------------------------

    def _get_json(self, path: str) -> Any:
        req = urllib.request.Request(f"{self.base}{path}")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                return json.loads(resp.read())
        except urllib.error.HTTPError as e:
            raise MosaicError(
                f"GET {path} failed: {e.code} {e.reason}", status=e.code
            ) from None

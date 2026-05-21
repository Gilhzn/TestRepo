"""Mosaic SDK for Python.

Talks to a running ``mosaic-serve`` instance over HTTP. Designed for AI
agents and tooling written in Python — pairs with the Rust crate
(``mosaic-sdk``) and the TypeScript package (``mosaic-sdk``).

Stdlib-only: no external dependencies. Uses ``urllib`` for HTTP and
``json`` for parsing.
"""

from .client import (
    ApplyReport,
    BranchSummary,
    ChangeDetail,
    ChangeSummary,
    FileDiff,
    HealthResponse,
    MosaicClient,
    MosaicError,
)
from .live import LiveSession

__version__ = "0.0.1"

__all__ = [
    "ApplyReport",
    "BranchSummary",
    "ChangeDetail",
    "ChangeSummary",
    "FileDiff",
    "HealthResponse",
    "LiveSession",
    "MosaicClient",
    "MosaicError",
]

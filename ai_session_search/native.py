"""Typed access to the Rust AI Session Search service.

This module is available in wheels built with maturin. Keeping the extension in
``_native`` prevents binding details from becoming the public API namespace.
"""

from ._native import NativeMessageHit, RefreshOutcome, SessionSearch

__all__ = ["NativeMessageHit", "RefreshOutcome", "SessionSearch"]

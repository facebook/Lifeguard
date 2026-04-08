import io
from typing import Any, Protocol

class _IOBytes(Protocol):
    def read(self, size: int, /) -> bytes: no_effects()
    def seek(self, size: int, whence: int = ..., /) -> Any: unsafe()

def load_tzdata(key: str) -> io.BufferedReader: unsafe()
def load_data(
    fobj: _IOBytes,
) -> tuple[tuple[int, ...], tuple[int, ...], tuple[int, ...], tuple[int, ...], tuple[str, ...], bytes | None]: unsafe()

class ZoneInfoNotFoundError(KeyError): ...

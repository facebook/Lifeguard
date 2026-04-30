from _typeshed import ReadableBuffer, WriteableBuffer
from collections.abc import Iterator
from typing import Any

def pack(fmt: str | bytes, /, *v: Any) -> bytes: no_effects()
def pack_into(fmt: str | bytes, buffer: WriteableBuffer, offset: int, /, *v: Any) -> None: no_effects()
def unpack(format: str | bytes, buffer: ReadableBuffer, /) -> tuple[Any, ...]: no_effects()
def unpack_from(format: str | bytes, /, buffer: ReadableBuffer, offset: int = 0) -> tuple[Any, ...]: no_effects()
def iter_unpack(format: str | bytes, buffer: ReadableBuffer, /) -> Iterator[tuple[Any, ...]]: no_effects()
def calcsize(format: str | bytes, /) -> int: no_effects()

class Struct:
    @property
    def format(self) -> str: ...
    @property
    def size(self) -> int: ...
    def __init__(self, format: str | bytes) -> None: no_effects()
    def pack(self, *v: Any) -> bytes: no_effects()
    def pack_into(self, buffer: WriteableBuffer, offset: int, *v: Any) -> None: no_effects()
    def unpack(self, buffer: ReadableBuffer, /) -> tuple[Any, ...]: no_effects()
    def unpack_from(self, buffer: ReadableBuffer, offset: int = 0) -> tuple[Any, ...]: no_effects()
    def iter_unpack(self, buffer: ReadableBuffer, /) -> Iterator[tuple[Any, ...]]: no_effects()

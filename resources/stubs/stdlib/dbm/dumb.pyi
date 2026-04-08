import sys
from _typeshed import StrOrBytesPath
from collections.abc import Iterator, MutableMapping
from types import TracebackType
from typing_extensions import Self, TypeAlias

__all__ = ["error", "open"]

_KeyType: TypeAlias = str | bytes
_ValueType: TypeAlias = str | bytes

error = OSError

# This class doesn't exist at runtime. open() can return an instance of
# any of the three implementations of dbm (dumb, gnu, ndbm), and this
# class is intended to represent the common interface supported by all three.
class _Database(MutableMapping[_KeyType, bytes]):
    def __init__(self, filebasename: str, mode: str, flag: str = "c") -> None: unsafe()
    def sync(self) -> None: unsafe()
    def iterkeys(self) -> Iterator[bytes]: unsafe()  # undocumented
    def close(self) -> None: unsafe()
    def __getitem__(self, key: _KeyType) -> bytes: unsafe()
    def __setitem__(self, key: _KeyType, val: _ValueType) -> None: unsafe()
    def __delitem__(self, key: _KeyType) -> None: unsafe()
    def __iter__(self) -> Iterator[bytes]: unsafe()
    def __len__(self) -> int: unsafe()
    def __del__(self) -> None: unsafe()
    def __enter__(self) -> Self: no_effects()
    def __exit__(
        self, exc_type: type[BaseException] | None, exc_val: BaseException | None, exc_tb: TracebackType | None
    ) -> None: unsafe()

if sys.version_info >= (3, 11):
    def open(file: StrOrBytesPath, flag: str = "c", mode: int = 0o666) -> _Database: unsafe()

else:
    def open(file: str, flag: str = "c", mode: int = 0o666) -> _Database: unsafe()

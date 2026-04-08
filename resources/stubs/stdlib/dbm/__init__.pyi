import sys
from _typeshed import StrOrBytesPath
from collections.abc import Iterator, MutableMapping
from types import TracebackType
from typing import Literal, type_check_only
from typing_extensions import Self, TypeAlias

__all__ = ["open", "whichdb", "error"]

_KeyType: TypeAlias = str | bytes
_ValueType: TypeAlias = str | bytes | bytearray
_TFlags: TypeAlias = Literal[
    "r",
    "w",
    "c",
    "n",
    "rf",
    "wf",
    "cf",
    "nf",
    "rs",
    "ws",
    "cs",
    "ns",
    "ru",
    "wu",
    "cu",
    "nu",
    "rfs",
    "wfs",
    "cfs",
    "nfs",
    "rfu",
    "wfu",
    "cfu",
    "nfu",
    "rsf",
    "wsf",
    "csf",
    "nsf",
    "rsu",
    "wsu",
    "csu",
    "nsu",
    "ruf",
    "wuf",
    "cuf",
    "nuf",
    "rus",
    "wus",
    "cus",
    "nus",
    "rfsu",
    "wfsu",
    "cfsu",
    "nfsu",
    "rfus",
    "wfus",
    "cfus",
    "nfus",
    "rsfu",
    "wsfu",
    "csfu",
    "nsfu",
    "rsuf",
    "wsuf",
    "csuf",
    "nsuf",
    "rufs",
    "wufs",
    "cufs",
    "nufs",
    "rusf",
    "wusf",
    "cusf",
    "nusf",
]

class _Database(MutableMapping[_KeyType, bytes]):
    def close(self) -> None: unsafe()
    def __getitem__(self, key: _KeyType) -> bytes: unsafe()
    def __setitem__(self, key: _KeyType, value: _ValueType) -> None: unsafe()
    def __delitem__(self, key: _KeyType) -> None: unsafe()
    def __iter__(self) -> Iterator[bytes]: unsafe()
    def __len__(self) -> int: unsafe()
    def __del__(self) -> None: unsafe()
    def __enter__(self) -> Self: no_effects()
    def __exit__(
        self, exc_type: type[BaseException] | None, exc_val: BaseException | None, exc_tb: TracebackType | None
    ) -> None: unsafe()

# This class is not exposed. It calls itself dbm.error.
@type_check_only
class _error(Exception): ...

error: tuple[type[_error], type[OSError]]

if sys.version_info >= (3, 11):
    def whichdb(filename: StrOrBytesPath) -> str | None: unsafe()
    def open(file: StrOrBytesPath, flag: _TFlags = "r", mode: int = 0o666) -> _Database: unsafe()

else:
    def whichdb(filename: str) -> str | None: unsafe()
    def open(file: str, flag: _TFlags = "r", mode: int = 0o666) -> _Database: unsafe()

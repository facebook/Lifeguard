import builtins
import sys
from enum import Enum
from typing import Final
from typing_extensions import LiteralString, TypeAlias

_FieldsType: TypeAlias = tuple[int, int, int, int, int, int]

class SafeUUID(Enum):
    safe = 0
    unsafe = -1
    unknown = None

class UUID:
    def __init__(
        self,
        hex: str | None = None,
        bytes: builtins.bytes | None = None,
        bytes_le: builtins.bytes | None = None,
        fields: _FieldsType | None = None,
        int: builtins.int | None = None,
        version: builtins.int | None = None,
        *,
        is_safe: SafeUUID = ...,
    ) -> None: no_effects()
    @property
    def is_safe(self) -> SafeUUID: no_effects()
    @property
    def bytes(self) -> builtins.bytes: no_effects()
    @property
    def bytes_le(self) -> builtins.bytes: no_effects()
    @property
    def clock_seq(self) -> builtins.int: no_effects()
    @property
    def clock_seq_hi_variant(self) -> builtins.int: no_effects()
    @property
    def clock_seq_low(self) -> builtins.int: no_effects()
    @property
    def fields(self) -> _FieldsType: no_effects()
    @property
    def hex(self) -> str: no_effects()
    @property
    def int(self) -> builtins.int: no_effects()
    @property
    def node(self) -> builtins.int: no_effects()
    @property
    def time(self) -> builtins.int: no_effects()
    @property
    def time_hi_version(self) -> builtins.int: no_effects()
    @property
    def time_low(self) -> builtins.int: no_effects()
    @property
    def time_mid(self) -> builtins.int: no_effects()
    @property
    def urn(self) -> str: no_effects()
    @property
    def variant(self) -> str: no_effects()
    @property
    def version(self) -> builtins.int | None: no_effects()
    def __int__(self) -> builtins.int: no_effects()
    def __eq__(self, other: object) -> bool: no_effects()
    def __lt__(self, other: UUID) -> bool: no_effects()
    def __le__(self, other: UUID) -> bool: no_effects()
    def __gt__(self, other: UUID) -> bool: no_effects()
    def __ge__(self, other: UUID) -> bool: no_effects()
    def __hash__(self) -> builtins.int: no_effects()

def getnode() -> int: no_effects()
def uuid1(node: int | None = None, clock_seq: int | None = None) -> UUID: no_effects()

if sys.version_info >= (3, 14):
    def uuid6(node: int | None = None, clock_seq: int | None = None) -> UUID: no_effects()
    def uuid7() -> UUID: no_effects()
    def uuid8(a: int | None = None, b: int | None = None, c: int | None = None) -> UUID: no_effects()

if sys.version_info >= (3, 12):
    def uuid3(namespace: UUID, name: str | bytes) -> UUID: no_effects()

else:
    def uuid3(namespace: UUID, name: str) -> UUID: no_effects()

def uuid4() -> UUID: no_effects()

if sys.version_info >= (3, 12):
    def uuid5(namespace: UUID, name: str | bytes) -> UUID: no_effects()

else:
    def uuid5(namespace: UUID, name: str) -> UUID: no_effects()

if sys.version_info >= (3, 14):
    NIL: Final[UUID]
    MAX: Final[UUID]

NAMESPACE_DNS: Final[UUID]
NAMESPACE_URL: Final[UUID]
NAMESPACE_OID: Final[UUID]
NAMESPACE_X500: Final[UUID]
RESERVED_NCS: Final[LiteralString]
RFC_4122: Final[LiteralString]
RESERVED_MICROSOFT: Final[LiteralString]
RESERVED_FUTURE: Final[LiteralString]

if sys.version_info >= (3, 12):
    def main() -> None: no_effects()

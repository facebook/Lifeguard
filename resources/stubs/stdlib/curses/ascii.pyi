from typing import TypeVar

_CharT = TypeVar("_CharT", str, int)

NUL: int
SOH: int
STX: int
ETX: int
EOT: int
ENQ: int
ACK: int
BEL: int
BS: int
TAB: int
HT: int
LF: int
NL: int
VT: int
FF: int
CR: int
SO: int
SI: int
DLE: int
DC1: int
DC2: int
DC3: int
DC4: int
NAK: int
SYN: int
ETB: int
CAN: int
EM: int
SUB: int
ESC: int
FS: int
GS: int
RS: int
US: int
SP: int
DEL: int

controlnames: list[int]

def isalnum(c: str | int) -> bool: no_effects()
def isalpha(c: str | int) -> bool: no_effects()
def isascii(c: str | int) -> bool: no_effects()
def isblank(c: str | int) -> bool: no_effects()
def iscntrl(c: str | int) -> bool: no_effects()
def isdigit(c: str | int) -> bool: no_effects()
def isgraph(c: str | int) -> bool: no_effects()
def islower(c: str | int) -> bool: no_effects()
def isprint(c: str | int) -> bool: no_effects()
def ispunct(c: str | int) -> bool: no_effects()
def isspace(c: str | int) -> bool: no_effects()
def isupper(c: str | int) -> bool: no_effects()
def isxdigit(c: str | int) -> bool: no_effects()
def isctrl(c: str | int) -> bool: no_effects()
def ismeta(c: str | int) -> bool: no_effects()
def ascii(c: _CharT) -> _CharT: no_effects()
def ctrl(c: _CharT) -> _CharT: no_effects()
def alt(c: _CharT) -> _CharT: no_effects()
def unctrl(c: str | int) -> str: no_effects()

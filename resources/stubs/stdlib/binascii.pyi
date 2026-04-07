import sys
from _typeshed import ReadableBuffer
from typing_extensions import TypeAlias

# Many functions in binascii accept buffer objects
# or ASCII-only strings.
_AsciiBuffer: TypeAlias = str | ReadableBuffer

def a2b_uu(data: _AsciiBuffer, /) -> bytes: no_effects()
def b2a_uu(data: ReadableBuffer, /, *, backtick: bool = False) -> bytes: no_effects()

if sys.version_info >= (3, 11):
    def a2b_base64(data: _AsciiBuffer, /, *, strict_mode: bool = False) -> bytes: no_effects()

else:
    def a2b_base64(data: _AsciiBuffer, /) -> bytes: no_effects()

def b2a_base64(data: ReadableBuffer, /, *, newline: bool = True) -> bytes: no_effects()
def a2b_qp(data: _AsciiBuffer, header: bool = False) -> bytes: no_effects()
def b2a_qp(data: ReadableBuffer, quotetabs: bool = False, istext: bool = True, header: bool = False) -> bytes: no_effects()

if sys.version_info < (3, 11):
    def a2b_hqx(data: _AsciiBuffer, /) -> bytes: no_effects()
    def rledecode_hqx(data: ReadableBuffer, /) -> bytes: no_effects()
    def rlecode_hqx(data: ReadableBuffer, /) -> bytes: no_effects()
    def b2a_hqx(data: ReadableBuffer, /) -> bytes: no_effects()

def crc_hqx(data: ReadableBuffer, crc: int, /) -> int: no_effects()
def crc32(data: ReadableBuffer, crc: int = 0, /) -> int: no_effects()
def b2a_hex(data: ReadableBuffer, sep: str | bytes = ..., bytes_per_sep: int = ...) -> bytes: no_effects()
def hexlify(data: ReadableBuffer, sep: str | bytes = ..., bytes_per_sep: int = ...) -> bytes: no_effects()
def a2b_hex(hexstr: _AsciiBuffer, /) -> bytes: no_effects()
def unhexlify(hexstr: _AsciiBuffer, /) -> bytes: no_effects()

class Error(ValueError): ...
class Incomplete(Exception): ...

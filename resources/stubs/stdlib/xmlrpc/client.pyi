import gzip
import http.client
import time
from _typeshed import ReadableBuffer, SizedBuffer, SupportsRead, SupportsWrite
from collections.abc import Callable, Iterable, Mapping
from datetime import datetime
from io import BytesIO
from types import TracebackType
from typing import Any, ClassVar, Final, Literal, Protocol, overload
from typing_extensions import Self, TypeAlias

class _SupportsTimeTuple(Protocol):
    def timetuple(self) -> time.struct_time: no_effects()

_DateTimeComparable: TypeAlias = DateTime | datetime | str | _SupportsTimeTuple
_Marshallable: TypeAlias = (
    bool
    | int
    | float
    | str
    | bytes
    | bytearray
    | None
    | tuple[_Marshallable, ...]
    # Ideally we'd use _Marshallable for list and dict, but invariance makes that impractical
    | list[Any]
    | dict[str, Any]
    | datetime
    | DateTime
    | Binary
)
_XMLDate: TypeAlias = int | datetime | tuple[int, ...] | time.struct_time
_HostType: TypeAlias = tuple[str, dict[str, str]] | str

def escape(s: str) -> str: no_effects()  # undocumented

MAXINT: Final[int]  # undocumented
MININT: Final[int]  # undocumented

PARSE_ERROR: Final[int]  # undocumented
SERVER_ERROR: Final[int]  # undocumented
APPLICATION_ERROR: Final[int]  # undocumented
SYSTEM_ERROR: Final[int]  # undocumented
TRANSPORT_ERROR: Final[int]  # undocumented

NOT_WELLFORMED_ERROR: Final[int]  # undocumented
UNSUPPORTED_ENCODING: Final[int]  # undocumented
INVALID_ENCODING_CHAR: Final[int]  # undocumented
INVALID_XMLRPC: Final[int]  # undocumented
METHOD_NOT_FOUND: Final[int]  # undocumented
INVALID_METHOD_PARAMS: Final[int]  # undocumented
INTERNAL_ERROR: Final[int]  # undocumented

class Error(Exception): ...

class ProtocolError(Error):
    url: str
    errcode: int
    errmsg: str
    headers: dict[str, str]
    def __init__(self, url: str, errcode: int, errmsg: str, headers: dict[str, str]) -> None: no_effects()

class ResponseError(Error): ...

class Fault(Error):
    faultCode: int
    faultString: str
    def __init__(self, faultCode: int, faultString: str, **extra: Any) -> None: no_effects()

boolean = bool
Boolean = bool

def _iso8601_format(value: datetime) -> str: no_effects()  # undocumented
def _strftime(value: _XMLDate) -> str: no_effects()  # undocumented

class DateTime:
    value: str  # undocumented
    def __init__(self, value: int | str | datetime | time.struct_time | tuple[int, ...] = 0) -> None: no_effects()
    __hash__: ClassVar[None]  # type: ignore[assignment]
    def __lt__(self, other: _DateTimeComparable) -> bool: no_effects()
    def __le__(self, other: _DateTimeComparable) -> bool: no_effects()
    def __gt__(self, other: _DateTimeComparable) -> bool: no_effects()
    def __ge__(self, other: _DateTimeComparable) -> bool: no_effects()
    def __eq__(self, other: _DateTimeComparable) -> bool: no_effects()  # type: ignore[override]
    def make_comparable(self, other: _DateTimeComparable) -> tuple[str, str]: no_effects()  # undocumented
    def timetuple(self) -> time.struct_time: no_effects()  # undocumented
    def decode(self, data: Any) -> None: mutation()
    def encode(self, out: SupportsWrite[str]) -> None: unsafe()

def _datetime(data: Any) -> DateTime: no_effects()  # undocumented
def _datetime_type(data: str) -> datetime: no_effects()  # undocumented

class Binary:
    data: bytes
    def __init__(self, data: bytes | bytearray | None = None) -> None: no_effects()
    def decode(self, data: ReadableBuffer) -> None: mutation()
    def encode(self, out: SupportsWrite[str]) -> None: unsafe()
    def __eq__(self, other: object) -> bool: no_effects()
    __hash__: ClassVar[None]  # type: ignore[assignment]

def _binary(data: ReadableBuffer) -> Binary: no_effects()  # undocumented

WRAPPERS: Final[tuple[type[DateTime], type[Binary]]]  # undocumented

class ExpatParser:  # undocumented
    def __init__(self, target: Unmarshaller) -> None: no_effects()
    def feed(self, data: str | ReadableBuffer) -> None: mutation()
    def close(self) -> None: mutation()

_WriteCallback: TypeAlias = Callable[[str], object]

class Marshaller:
    dispatch: dict[type[_Marshallable] | Literal["_arbitrary_instance"], Callable[[Marshaller, Any, _WriteCallback], None]]
    memo: dict[Any, None]
    data: None
    encoding: str | None
    allow_none: bool
    def __init__(self, encoding: str | None = None, allow_none: bool = False) -> None: no_effects()
    def dumps(self, values: Fault | Iterable[_Marshallable]) -> str: no_effects()
    def __dump(self, value: _Marshallable, write: _WriteCallback) -> None: no_effects()  # undocumented
    def dump_nil(self, value: None, write: _WriteCallback) -> None: no_effects()
    def dump_bool(self, value: bool, write: _WriteCallback) -> None: no_effects()
    def dump_long(self, value: int, write: _WriteCallback) -> None: no_effects()
    def dump_int(self, value: int, write: _WriteCallback) -> None: no_effects()
    def dump_double(self, value: float, write: _WriteCallback) -> None: no_effects()
    def dump_unicode(self, value: str, write: _WriteCallback, escape: Callable[[str], str] = ...) -> None: no_effects()
    def dump_bytes(self, value: ReadableBuffer, write: _WriteCallback) -> None: no_effects()
    def dump_array(self, value: Iterable[_Marshallable], write: _WriteCallback) -> None: no_effects()
    def dump_struct(
        self, value: Mapping[str, _Marshallable], write: _WriteCallback, escape: Callable[[str], str] = ...
    ) -> None: no_effects()
    def dump_datetime(self, value: _XMLDate, write: _WriteCallback) -> None: no_effects()
    def dump_instance(self, value: object, write: _WriteCallback) -> None: no_effects()

class Unmarshaller:
    dispatch: dict[str, Callable[[Unmarshaller, str], None]]

    _type: str | None
    _stack: list[_Marshallable]
    _marks: list[int]
    _data: list[str]
    _value: bool
    _methodname: str | None
    _encoding: str
    append: Callable[[Any], None]
    _use_datetime: bool
    _use_builtin_types: bool
    def __init__(self, use_datetime: bool = False, use_builtin_types: bool = False) -> None: no_effects()
    def close(self) -> tuple[_Marshallable, ...]: no_effects()
    def getmethodname(self) -> str | None: no_effects()
    def xml(self, encoding: str, standalone: Any) -> None: mutation()  # Standalone is ignored
    def start(self, tag: str, attrs: dict[str, str]) -> None: mutation()
    def data(self, text: str) -> None: mutation()
    def end(self, tag: str) -> None: mutation()
    def end_dispatch(self, tag: str, data: str) -> None: mutation()
    def end_nil(self, data: str) -> None: mutation()
    def end_boolean(self, data: str) -> None: mutation()
    def end_int(self, data: str) -> None: mutation()
    def end_double(self, data: str) -> None: mutation()
    def end_bigdecimal(self, data: str) -> None: mutation()
    def end_string(self, data: str) -> None: mutation()
    def end_array(self, data: str) -> None: mutation()
    def end_struct(self, data: str) -> None: mutation()
    def end_base64(self, data: str) -> None: mutation()
    def end_dateTime(self, data: str) -> None: mutation()
    def end_value(self, data: str) -> None: mutation()
    def end_params(self, data: str) -> None: mutation()
    def end_fault(self, data: str) -> None: mutation()
    def end_methodName(self, data: str) -> None: mutation()

class _MultiCallMethod:  # undocumented
    __call_list: list[tuple[str, tuple[_Marshallable, ...]]]
    __name: str
    def __init__(self, call_list: list[tuple[str, _Marshallable]], name: str) -> None: no_effects()
    def __getattr__(self, name: str) -> _MultiCallMethod: no_effects()
    def __call__(self, *args: _Marshallable) -> None: mutation()

class MultiCallIterator:  # undocumented
    results: list[list[_Marshallable]]
    def __init__(self, results: list[list[_Marshallable]]) -> None: no_effects()
    def __getitem__(self, i: int) -> _Marshallable: no_effects()

class MultiCall:
    __server: ServerProxy
    __call_list: list[tuple[str, tuple[_Marshallable, ...]]]
    def __init__(self, server: ServerProxy) -> None: no_effects()
    def __getattr__(self, name: str) -> _MultiCallMethod: no_effects()
    def __call__(self) -> MultiCallIterator: unsafe()

# A little white lie
FastMarshaller: Marshaller | None
FastParser: ExpatParser | None
FastUnmarshaller: Unmarshaller | None

def getparser(use_datetime: bool = False, use_builtin_types: bool = False) -> tuple[ExpatParser, Unmarshaller]: no_effects()
def dumps(
    params: Fault | tuple[_Marshallable, ...],
    methodname: str | None = None,
    methodresponse: bool | None = None,
    encoding: str | None = None,
    allow_none: bool = False,
) -> str: no_effects()
def loads(
    data: str | ReadableBuffer, use_datetime: bool = False, use_builtin_types: bool = False
) -> tuple[tuple[_Marshallable, ...], str | None]: no_effects()
def gzip_encode(data: ReadableBuffer) -> bytes: no_effects()  # undocumented
def gzip_decode(data: ReadableBuffer, max_decode: int = 20971520) -> bytes: no_effects()  # undocumented

class GzipDecodedResponse(gzip.GzipFile):  # undocumented
    io: BytesIO
    def __init__(self, response: SupportsRead[ReadableBuffer]) -> None: no_effects()

class _Method:  # undocumented
    __send: Callable[[str, tuple[_Marshallable, ...]], _Marshallable]
    __name: str
    def __init__(self, send: Callable[[str, tuple[_Marshallable, ...]], _Marshallable], name: str) -> None: no_effects()
    def __getattr__(self, name: str) -> _Method: no_effects()
    def __call__(self, *args: _Marshallable) -> _Marshallable: unsafe()

class Transport:
    user_agent: str
    accept_gzip_encoding: bool
    encode_threshold: int | None

    _use_datetime: bool
    _use_builtin_types: bool
    _connection: tuple[_HostType | None, http.client.HTTPConnection | None]
    _headers: list[tuple[str, str]]
    _extra_headers: list[tuple[str, str]]

    def __init__(
        self, use_datetime: bool = False, use_builtin_types: bool = False, *, headers: Iterable[tuple[str, str]] = ()
    ) -> None: no_effects()
    def request(
        self, host: _HostType, handler: str, request_body: SizedBuffer, verbose: bool = False
    ) -> tuple[_Marshallable, ...]: unsafe()
    def single_request(
        self, host: _HostType, handler: str, request_body: SizedBuffer, verbose: bool = False
    ) -> tuple[_Marshallable, ...]: unsafe()
    def getparser(self) -> tuple[ExpatParser, Unmarshaller]: no_effects()
    def get_host_info(self, host: _HostType) -> tuple[str, list[tuple[str, str]], dict[str, str]]: no_effects()
    def make_connection(self, host: _HostType) -> http.client.HTTPConnection: unsafe()
    def close(self) -> None: unsafe()
    def send_request(
        self, host: _HostType, handler: str, request_body: SizedBuffer, debug: bool
    ) -> http.client.HTTPConnection: unsafe()
    def send_headers(self, connection: http.client.HTTPConnection, headers: list[tuple[str, str]]) -> None: unsafe()
    def send_content(self, connection: http.client.HTTPConnection, request_body: SizedBuffer) -> None: unsafe()
    def parse_response(self, response: http.client.HTTPResponse) -> tuple[_Marshallable, ...]: unsafe()

class SafeTransport(Transport):
    def __init__(
        self,
        use_datetime: bool = False,
        use_builtin_types: bool = False,
        *,
        headers: Iterable[tuple[str, str]] = (),
        context: Any | None = None,
    ) -> None: no_effects()
    def make_connection(self, host: _HostType) -> http.client.HTTPSConnection: unsafe()

class ServerProxy:
    __host: str
    __handler: str
    __transport: Transport
    __encoding: str
    __verbose: bool
    __allow_none: bool

    def __init__(
        self,
        uri: str,
        transport: Transport | None = None,
        encoding: str | None = None,
        verbose: bool = False,
        allow_none: bool = False,
        use_datetime: bool = False,
        use_builtin_types: bool = False,
        *,
        headers: Iterable[tuple[str, str]] = (),
        context: Any | None = None,
    ) -> None: no_effects()
    def __getattr__(self, name: str) -> _Method: no_effects()
    @overload
    def __call__(self, attr: Literal["close"]) -> Callable[[], None]: no_effects()
    @overload
    def __call__(self, attr: Literal["transport"]) -> Transport: ...
    @overload
    def __call__(self, attr: str) -> Callable[[], None] | Transport: ...
    def __enter__(self) -> Self: no_effects()
    def __exit__(
        self, exc_type: type[BaseException] | None, exc_val: BaseException | None, exc_tb: TracebackType | None
    ) -> None: unsafe()
    def __close(self) -> None: unsafe()  # undocumented
    def __request(self, methodname: str, params: tuple[_Marshallable, ...]) -> tuple[_Marshallable, ...]: unsafe()  # undocumented

Server = ServerProxy

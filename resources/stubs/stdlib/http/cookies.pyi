from collections.abc import Iterable, Mapping
from types import GenericAlias
from typing import Any, Generic, TypeVar, overload
from typing_extensions import TypeAlias

__all__ = ["CookieError", "BaseCookie", "SimpleCookie"]

_DataType: TypeAlias = str | Mapping[str, str | Morsel[Any]]
_T = TypeVar("_T")

@overload
def _quote(str: None) -> None: no_effects()
@overload
def _quote(str: str) -> str: ...
@overload
def _unquote(str: None) -> None: no_effects()
@overload
def _unquote(str: str) -> str: ...

class CookieError(Exception): ...

class Morsel(dict[str, Any], Generic[_T]):
    @property
    def value(self) -> str: no_effects()
    @property
    def coded_value(self) -> _T: no_effects()
    @property
    def key(self) -> str: no_effects()
    def __init__(self) -> None: no_effects()
    def set(self, key: str, val: str, coded_val: _T) -> None: mutation()
    def setdefault(self, key: str, val: str | None = None) -> str: mutation()
    # The dict update can also get a keywords argument so this is incompatible
    @overload  # type: ignore[override]
    def update(self, values: Mapping[str, str]) -> None: mutation()
    @overload
    def update(self, values: Iterable[tuple[str, str]]) -> None: ...
    def isReservedKey(self, K: str) -> bool: no_effects()
    def output(self, attrs: list[str] | None = None, header: str = "Set-Cookie:") -> str: no_effects()
    __str__ = output
    def js_output(self, attrs: list[str] | None = None) -> str: no_effects()
    def OutputString(self, attrs: list[str] | None = None) -> str: no_effects()
    def __eq__(self, morsel: object) -> bool: ...
    def __setitem__(self, K: str, V: Any) -> None: ...
    def __class_getitem__(cls, item: Any, /) -> GenericAlias: ...

class BaseCookie(dict[str, Morsel[_T]], Generic[_T]):
    def __init__(self, input: _DataType | None = None) -> None: no_effects()
    def value_decode(self, val: str) -> tuple[_T, str]: no_effects()
    def value_encode(self, val: _T) -> tuple[_T, str]: no_effects()
    def output(self, attrs: list[str] | None = None, header: str = "Set-Cookie:", sep: str = "\r\n") -> str: no_effects()
    __str__ = output
    def js_output(self, attrs: list[str] | None = None) -> str: no_effects()
    def load(self, rawdata: _DataType) -> None: mutation()
    def __setitem__(self, key: str, value: str | Morsel[_T]) -> None: ...

class SimpleCookie(BaseCookie[str]): ...

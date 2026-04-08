import sys
from _typeshed import StrOrLiteralStr
from collections.abc import Iterable, Mapping, Sequence
from re import Pattern, RegexFlag
from typing import Any, ClassVar, overload
from typing_extensions import LiteralString

__all__ = [
    "ascii_letters",
    "ascii_lowercase",
    "ascii_uppercase",
    "capwords",
    "digits",
    "hexdigits",
    "octdigits",
    "printable",
    "punctuation",
    "whitespace",
    "Formatter",
    "Template",
]

ascii_letters: LiteralString
ascii_lowercase: LiteralString
ascii_uppercase: LiteralString
digits: LiteralString
hexdigits: LiteralString
octdigits: LiteralString
punctuation: LiteralString
printable: LiteralString
whitespace: LiteralString

def capwords(s: StrOrLiteralStr, sep: StrOrLiteralStr | None = None) -> StrOrLiteralStr: no_effects()

class Template:
    template: str
    delimiter: ClassVar[str]
    idpattern: ClassVar[str]
    braceidpattern: ClassVar[str | None]
    if sys.version_info >= (3, 14):
        flags: ClassVar[RegexFlag | None]
    else:
        flags: ClassVar[RegexFlag]
    pattern: ClassVar[Pattern[str]]
    def __init__(self, template: str) -> None: no_effects()
    def substitute(self, mapping: Mapping[str, object] = {}, /, **kwds: object) -> str: no_effects()
    def safe_substitute(self, mapping: Mapping[str, object] = {}, /, **kwds: object) -> str: no_effects()
    if sys.version_info >= (3, 11):
        def get_identifiers(self) -> list[str]: no_effects()
        def is_valid(self) -> bool: no_effects()

class Formatter:
    @overload
    def format(self, format_string: LiteralString, /, *args: LiteralString, **kwargs: LiteralString) -> LiteralString: no_effects()
    @overload
    def format(self, format_string: str, /, *args: Any, **kwargs: Any) -> str: ...
    @overload
    def vformat(
        self, format_string: LiteralString, args: Sequence[LiteralString], kwargs: Mapping[LiteralString, LiteralString]
    ) -> LiteralString: no_effects()
    @overload
    def vformat(self, format_string: str, args: Sequence[Any], kwargs: Mapping[str, Any]) -> str: ...
    def _vformat(  # undocumented
        self,
        format_string: str,
        args: Sequence[Any],
        kwargs: Mapping[str, Any],
        used_args: set[int | str],
        recursion_depth: int,
        auto_arg_index: int = 0,
    ) -> tuple[str, int]: no_effects()
    def parse(
        self, format_string: StrOrLiteralStr
    ) -> Iterable[tuple[StrOrLiteralStr, StrOrLiteralStr | None, StrOrLiteralStr | None, StrOrLiteralStr | None]]: no_effects()
    def get_field(self, field_name: str, args: Sequence[Any], kwargs: Mapping[str, Any]) -> Any: no_effects()
    def get_value(self, key: int | str, args: Sequence[Any], kwargs: Mapping[str, Any]) -> Any: no_effects()
    def check_unused_args(self, used_args: set[int | str], args: Sequence[Any], kwargs: Mapping[str, Any]) -> None: no_effects()
    def format_field(self, value: Any, format_spec: str) -> Any: no_effects()
    def convert_field(self, value: Any, conversion: str | None) -> Any: no_effects()

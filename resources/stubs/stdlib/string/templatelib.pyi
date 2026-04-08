from collections.abc import Iterator
from types import GenericAlias
from typing import Any, Literal, final

__all__ = ["Interpolation", "Template"]

@final
class Template:  # TODO: consider making `Template` generic on `TypeVarTuple`
    strings: tuple[str, ...]
    interpolations: tuple[Interpolation, ...]

    def __new__(cls, *args: str | Interpolation) -> Template: no_effects()
    def __iter__(self) -> Iterator[str | Interpolation]: no_effects()
    def __add__(self, other: Template | str) -> Template: no_effects()
    def __class_getitem__(cls, item: Any, /) -> GenericAlias: no_effects()
    @property
    def values(self) -> tuple[Any, ...]: no_effects()  # Tuple of interpolation values, which can have any type

@final
class Interpolation:
    value: Any  # TODO: consider making `Interpolation` generic in runtime
    expression: str
    conversion: Literal["a", "r", "s"] | None
    format_spec: str

    __match_args__ = ("value", "expression", "conversion", "format_spec")

    def __new__(
        cls, value: Any, expression: str = "", conversion: Literal["a", "r", "s"] | None = None, format_spec: str = ""
    ) -> Interpolation: no_effects()
    def __class_getitem__(cls, item: Any, /) -> GenericAlias: no_effects()

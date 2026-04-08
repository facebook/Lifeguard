from _typeshed import StrPath
from collections.abc import Sequence

# Note: Both here and in clear_cache, the types allow the use of `str` where
# a sequence of strings is required. This should be remedied if a solution
# to this typing bug is found: https://github.com/python/typing/issues/256
def reset_tzpath(to: Sequence[StrPath] | None = None) -> None: unsafe()
def find_tzfile(key: str) -> str | None: no_effects()
def available_timezones() -> set[str]: no_effects()

TZPATH: tuple[str, ...]

class InvalidTZPathWarning(RuntimeWarning): ...

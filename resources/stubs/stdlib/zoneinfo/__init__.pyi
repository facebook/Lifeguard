import sys
from collections.abc import Iterable
from datetime import datetime, timedelta, tzinfo
from typing_extensions import Self
from zoneinfo._common import ZoneInfoNotFoundError as ZoneInfoNotFoundError, _IOBytes
from zoneinfo._tzpath import (
    TZPATH as TZPATH,
    InvalidTZPathWarning as InvalidTZPathWarning,
    available_timezones as available_timezones,
    reset_tzpath as reset_tzpath,
)

__all__ = ["ZoneInfo", "reset_tzpath", "available_timezones", "TZPATH", "ZoneInfoNotFoundError", "InvalidTZPathWarning"]

class ZoneInfo(tzinfo):
    @property
    def key(self) -> str: no_effects()
    def __new__(cls, key: str) -> Self: no_effects()
    @classmethod
    def no_cache(cls, key: str) -> Self: unsafe()
    if sys.version_info >= (3, 12):
        @classmethod
        def from_file(cls, file_obj: _IOBytes, /, key: str | None = None) -> Self: unsafe()
    else:
        @classmethod
        def from_file(cls, fobj: _IOBytes, /, key: str | None = None) -> Self: unsafe()

    @classmethod
    def clear_cache(cls, *, only_keys: Iterable[str] | None = None) -> None: mutation()
    def tzname(self, dt: datetime | None, /) -> str | None: no_effects()
    def utcoffset(self, dt: datetime | None, /) -> timedelta | None: no_effects()
    def dst(self, dt: datetime | None, /) -> timedelta | None: no_effects()

def __dir__() -> list[str]: no_effects()

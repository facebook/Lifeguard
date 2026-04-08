import sys
import types
from _typeshed import (
    OpenBinaryMode,
    OpenBinaryModeReading,
    OpenBinaryModeUpdating,
    OpenBinaryModeWriting,
    OpenTextMode,
    ReadableBuffer,
    StrOrBytesPath,
    StrPath,
    Unused,
)
from collections.abc import Callable, Generator, Iterator, Sequence
from io import BufferedRandom, BufferedReader, BufferedWriter, FileIO, TextIOWrapper
from os import PathLike, stat_result
from types import GenericAlias, TracebackType
from typing import IO, Any, BinaryIO, ClassVar, Literal, TypeVar, overload
from typing_extensions import Never, Self, deprecated

_PathT = TypeVar("_PathT", bound=PurePath)

__all__ = ["PurePath", "PurePosixPath", "PureWindowsPath", "Path", "PosixPath", "WindowsPath"]

if sys.version_info >= (3, 14):
    from pathlib.types import PathInfo

if sys.version_info >= (3, 13):
    __all__ += ["UnsupportedOperation"]

class PurePath(PathLike[str]):
    if sys.version_info >= (3, 13):
        parser: ClassVar[types.ModuleType]
        def full_match(self, pattern: StrPath, *, case_sensitive: bool | None = None) -> bool: no_effects()

    @property
    def parts(self) -> tuple[str, ...]: no_effects()
    @property
    def drive(self) -> str: no_effects()
    @property
    def root(self) -> str: no_effects()
    @property
    def anchor(self) -> str: no_effects()
    @property
    def name(self) -> str: no_effects()
    @property
    def suffix(self) -> str: no_effects()
    @property
    def suffixes(self) -> list[str]: no_effects()
    @property
    def stem(self) -> str: no_effects()
    if sys.version_info >= (3, 12):
        def __new__(cls, *args: StrPath, **kwargs: Unused) -> Self: no_effects()
        def __init__(self, *args: StrPath) -> None: no_effects()  # pyright: ignore[reportInconsistentConstructor]
    else:
        def __new__(cls, *args: StrPath) -> Self: no_effects()

    def __hash__(self) -> int: no_effects()
    def __fspath__(self) -> str: no_effects()
    def __lt__(self, other: PurePath) -> bool: no_effects()
    def __le__(self, other: PurePath) -> bool: no_effects()
    def __gt__(self, other: PurePath) -> bool: no_effects()
    def __ge__(self, other: PurePath) -> bool: no_effects()
    def __truediv__(self, key: StrPath) -> Self: no_effects()
    def __rtruediv__(self, key: StrPath) -> Self: no_effects()
    def __bytes__(self) -> bytes: no_effects()
    def as_posix(self) -> str: no_effects()
    def as_uri(self) -> str: no_effects()
    def is_absolute(self) -> bool: no_effects()
    def is_reserved(self) -> bool: no_effects()
    if sys.version_info >= (3, 14):
        def is_relative_to(self, other: StrPath) -> bool: no_effects()
    elif sys.version_info >= (3, 12):
        def is_relative_to(self, other: StrPath, /, *_deprecated: StrPath) -> bool: no_effects()
    else:
        def is_relative_to(self, *other: StrPath) -> bool: no_effects()

    if sys.version_info >= (3, 12):
        def match(self, path_pattern: str, *, case_sensitive: bool | None = None) -> bool: no_effects()
    else:
        def match(self, path_pattern: str) -> bool: no_effects()

    if sys.version_info >= (3, 14):
        def relative_to(self, other: StrPath, *, walk_up: bool = False) -> Self: no_effects()
    elif sys.version_info >= (3, 12):
        def relative_to(self, other: StrPath, /, *_deprecated: StrPath, walk_up: bool = False) -> Self: no_effects()
    else:
        def relative_to(self, *other: StrPath) -> Self: no_effects()

    def with_name(self, name: str) -> Self: no_effects()
    def with_stem(self, stem: str) -> Self: no_effects()
    def with_suffix(self, suffix: str) -> Self: no_effects()
    def joinpath(self, *other: StrPath) -> Self: no_effects()
    @property
    def parents(self) -> Sequence[Self]: no_effects()
    @property
    def parent(self) -> Self: no_effects()
    if sys.version_info < (3, 11):
        def __class_getitem__(cls, type: Any) -> GenericAlias: no_effects()

    if sys.version_info >= (3, 12):
        def with_segments(self, *args: StrPath) -> Self: no_effects()

class PurePosixPath(PurePath): ...
class PureWindowsPath(PurePath): ...

class Path(PurePath):
    if sys.version_info >= (3, 12):
        def __new__(cls, *args: StrPath, **kwargs: Unused) -> Self: no_effects()  # pyright: ignore[reportInconsistentConstructor]
    else:
        def __new__(cls, *args: StrPath, **kwargs: Unused) -> Self: no_effects()

    @classmethod
    def cwd(cls) -> Self: no_effects()
    if sys.version_info >= (3, 10):
        def stat(self, *, follow_symlinks: bool = True) -> stat_result: no_effects()
        def chmod(self, mode: int, *, follow_symlinks: bool = True) -> None: unsafe()
    else:
        def stat(self) -> stat_result: no_effects()
        def chmod(self, mode: int) -> None: unsafe()

    if sys.version_info >= (3, 13):
        @classmethod
        def from_uri(cls, uri: str) -> Self: no_effects()
        def is_dir(self, *, follow_symlinks: bool = True) -> bool: no_effects()
        def is_file(self, *, follow_symlinks: bool = True) -> bool: no_effects()
        def read_text(self, encoding: str | None = None, errors: str | None = None, newline: str | None = None) -> str: no_effects()
    else:
        def __enter__(self) -> Self: no_effects()
        def __exit__(self, t: type[BaseException] | None, v: BaseException | None, tb: TracebackType | None) -> None: no_effects()
        def is_dir(self) -> bool: no_effects()
        def is_file(self) -> bool: no_effects()
        def read_text(self, encoding: str | None = None, errors: str | None = None) -> str: no_effects()

    if sys.version_info >= (3, 13):
        def glob(self, pattern: str, *, case_sensitive: bool | None = None, recurse_symlinks: bool = False) -> Iterator[Self]: no_effects()
        def rglob(
            self, pattern: str, *, case_sensitive: bool | None = None, recurse_symlinks: bool = False
        ) -> Iterator[Self]: no_effects()
    elif sys.version_info >= (3, 12):
        def glob(self, pattern: str, *, case_sensitive: bool | None = None) -> Generator[Self, None, None]: no_effects()
        def rglob(self, pattern: str, *, case_sensitive: bool | None = None) -> Generator[Self, None, None]: no_effects()
    else:
        def glob(self, pattern: str) -> Generator[Self, None, None]: no_effects()
        def rglob(self, pattern: str) -> Generator[Self, None, None]: no_effects()

    if sys.version_info >= (3, 12):
        def exists(self, *, follow_symlinks: bool = True) -> bool: no_effects()
    else:
        def exists(self) -> bool: no_effects()

    def is_symlink(self) -> bool: no_effects()
    def is_socket(self) -> bool: no_effects()
    def is_fifo(self) -> bool: no_effects()
    def is_block_device(self) -> bool: no_effects()
    def is_char_device(self) -> bool: no_effects()
    if sys.version_info >= (3, 12):
        def is_junction(self) -> bool: no_effects()

    def iterdir(self) -> Generator[Self, None, None]: no_effects()
    def lchmod(self, mode: int) -> None: unsafe()
    def lstat(self) -> stat_result: no_effects()
    def mkdir(self, mode: int = 0o777, parents: bool = False, exist_ok: bool = False) -> None: unsafe()

    if sys.version_info >= (3, 14):

        @property
        def info(self) -> PathInfo: no_effects()
        @overload
        def move_into(self, target_dir: _PathT) -> _PathT: unsafe()  # type: ignore[overload-overlap]
        @overload
        def move_into(self, target_dir: StrPath) -> Self: ...  # type: ignore[overload-overlap]
        @overload
        def move(self, target: _PathT) -> _PathT: unsafe()  # type: ignore[overload-overlap]
        @overload
        def move(self, target: StrPath) -> Self: ...  # type: ignore[overload-overlap]
        @overload
        def copy_into(self, target_dir: _PathT, *, follow_symlinks: bool = True, preserve_metadata: bool = False) -> _PathT: unsafe()  # type: ignore[overload-overlap]
        @overload
        def copy_into(self, target_dir: StrPath, *, follow_symlinks: bool = True, preserve_metadata: bool = False) -> Self: ...  # type: ignore[overload-overlap]
        @overload
        def copy(self, target: _PathT, *, follow_symlinks: bool = True, preserve_metadata: bool = False) -> _PathT: unsafe()  # type: ignore[overload-overlap]
        @overload
        def copy(self, target: StrPath, *, follow_symlinks: bool = True, preserve_metadata: bool = False) -> Self: ...  # type: ignore[overload-overlap]

    # Adapted from builtins.open
    # Text mode: always returns a TextIOWrapper
    # The Traversable .open in stdlib/importlib/abc.pyi should be kept in sync with this.
    @overload
    def open(
        self,
        mode: OpenTextMode = "r",
        buffering: int = -1,
        encoding: str | None = None,
        errors: str | None = None,
        newline: str | None = None,
    ) -> TextIOWrapper: no_effects()
    # Unbuffered binary mode: returns a FileIO
    @overload
    def open(
        self, mode: OpenBinaryMode, buffering: Literal[0], encoding: None = None, errors: None = None, newline: None = None
    ) -> FileIO: ...
    # Buffering is on: return BufferedRandom, BufferedReader, or BufferedWriter
    @overload
    def open(
        self,
        mode: OpenBinaryModeUpdating,
        buffering: Literal[-1, 1] = -1,
        encoding: None = None,
        errors: None = None,
        newline: None = None,
    ) -> BufferedRandom: ...
    @overload
    def open(
        self,
        mode: OpenBinaryModeWriting,
        buffering: Literal[-1, 1] = -1,
        encoding: None = None,
        errors: None = None,
        newline: None = None,
    ) -> BufferedWriter: ...
    @overload
    def open(
        self,
        mode: OpenBinaryModeReading,
        buffering: Literal[-1, 1] = -1,
        encoding: None = None,
        errors: None = None,
        newline: None = None,
    ) -> BufferedReader: ...
    # Buffering cannot be determined: fall back to BinaryIO
    @overload
    def open(
        self, mode: OpenBinaryMode, buffering: int = -1, encoding: None = None, errors: None = None, newline: None = None
    ) -> BinaryIO: ...
    # Fallback if mode is not specified
    @overload
    def open(
        self, mode: str, buffering: int = -1, encoding: str | None = None, errors: str | None = None, newline: str | None = None
    ) -> IO[Any]: ...

    # These methods do "exist" on Windows on <3.13, but they always raise NotImplementedError.
    if sys.platform == "win32":
        if sys.version_info < (3, 13):
            def owner(self: Never) -> str: no_effects()  # type: ignore[misc]
            def group(self: Never) -> str: no_effects()  # type: ignore[misc]
    else:
        if sys.version_info >= (3, 13):
            def owner(self, *, follow_symlinks: bool = True) -> str: no_effects()
            def group(self, *, follow_symlinks: bool = True) -> str: no_effects()
        else:
            def owner(self) -> str: no_effects()
            def group(self) -> str: no_effects()

    # This method does "exist" on Windows on <3.12, but always raises NotImplementedError
    # On py312+, it works properly on Windows, as with all other platforms
    if sys.platform == "win32" and sys.version_info < (3, 12):
        def is_mount(self: Never) -> bool: no_effects()  # type: ignore[misc]
    else:
        def is_mount(self) -> bool: no_effects()

    def readlink(self) -> Self: no_effects()

    if sys.version_info >= (3, 10):
        def rename(self, target: StrPath) -> Self: unsafe()
        def replace(self, target: StrPath) -> Self: unsafe()
    else:
        def rename(self, target: str | PurePath) -> Self: unsafe()
        def replace(self, target: str | PurePath) -> Self: unsafe()

    def resolve(self, strict: bool = False) -> Self: no_effects()
    def rmdir(self) -> None: unsafe()
    def symlink_to(self, target: StrOrBytesPath, target_is_directory: bool = False) -> None: unsafe()
    if sys.version_info >= (3, 10):
        def hardlink_to(self, target: StrOrBytesPath) -> None: unsafe()

    def touch(self, mode: int = 0o666, exist_ok: bool = True) -> None: unsafe()
    def unlink(self, missing_ok: bool = False) -> None: unsafe()
    @classmethod
    def home(cls) -> Self: no_effects()
    def absolute(self) -> Self: no_effects()
    def expanduser(self) -> Self: no_effects()
    def read_bytes(self) -> bytes: no_effects()
    def samefile(self, other_path: StrPath) -> bool: no_effects()
    def write_bytes(self, data: ReadableBuffer) -> int: unsafe()
    if sys.version_info >= (3, 10):
        def write_text(
            self, data: str, encoding: str | None = None, errors: str | None = None, newline: str | None = None
        ) -> int: unsafe()
    else:
        def write_text(self, data: str, encoding: str | None = None, errors: str | None = None) -> int: unsafe()
    if sys.version_info < (3, 12):
        if sys.version_info >= (3, 10):
            @deprecated("Deprecated as of Python 3.10 and removed in Python 3.12. Use hardlink_to() instead.")
            def link_to(self, target: StrOrBytesPath) -> None: unsafe()
        else:
            def link_to(self, target: StrOrBytesPath) -> None: unsafe()
    if sys.version_info >= (3, 12):
        def walk(
            self, top_down: bool = ..., on_error: Callable[[OSError], object] | None = ..., follow_symlinks: bool = ...
        ) -> Iterator[tuple[Self, list[str], list[str]]]: no_effects()

class PosixPath(Path, PurePosixPath): ...
class WindowsPath(Path, PureWindowsPath): ...

if sys.version_info >= (3, 13):
    class UnsupportedOperation(NotImplementedError): ...

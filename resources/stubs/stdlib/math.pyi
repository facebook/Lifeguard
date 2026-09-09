import sys
from _typeshed import SupportsMul, SupportsRMul
from collections.abc import Iterable
from typing import Any, Final, Literal, Protocol, SupportsFloat, SupportsIndex, TypeVar, overload
from typing_extensions import TypeAlias

_T = TypeVar("_T")
_T_co = TypeVar("_T_co", covariant=True)

_SupportsFloatOrIndex: TypeAlias = SupportsFloat | SupportsIndex

e: Final[float]
pi: Final[float]
inf: Final[float]
nan: Final[float]
tau: Final[float]

def acos(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def acosh(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def asin(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def asinh(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def atan(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def atan2(y: _SupportsFloatOrIndex, x: _SupportsFloatOrIndex, /) -> float: no_effects()
def atanh(x: _SupportsFloatOrIndex, /) -> float: no_effects()

if sys.version_info >= (3, 11):
    def cbrt(x: _SupportsFloatOrIndex, /) -> float: no_effects()

class _SupportsCeil(Protocol[_T_co]):
    def __ceil__(self) -> _T_co: ...

@overload
def ceil(x: _SupportsCeil[_T], /) -> _T: no_effects()
@overload
def ceil(x: _SupportsFloatOrIndex, /) -> int: ...
def comb(n: SupportsIndex, k: SupportsIndex, /) -> int: no_effects()
def copysign(x: _SupportsFloatOrIndex, y: _SupportsFloatOrIndex, /) -> float: no_effects()
def cos(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def cosh(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def degrees(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def dist(p: Iterable[_SupportsFloatOrIndex], q: Iterable[_SupportsFloatOrIndex], /) -> float: no_effects()
def erf(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def erfc(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def exp(x: _SupportsFloatOrIndex, /) -> float: no_effects()

if sys.version_info >= (3, 11):
    def exp2(x: _SupportsFloatOrIndex, /) -> float: no_effects()

def expm1(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def fabs(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def factorial(x: SupportsIndex, /) -> int: no_effects()

class _SupportsFloor(Protocol[_T_co]):
    def __floor__(self) -> _T_co: ...

@overload
def floor(x: _SupportsFloor[_T], /) -> _T: no_effects()
@overload
def floor(x: _SupportsFloatOrIndex, /) -> int: ...
def fmod(x: _SupportsFloatOrIndex, y: _SupportsFloatOrIndex, /) -> float: no_effects()
def frexp(x: _SupportsFloatOrIndex, /) -> tuple[float, int]: no_effects()
def fsum(seq: Iterable[_SupportsFloatOrIndex], /) -> float: no_effects()
def gamma(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def gcd(*integers: SupportsIndex) -> int: no_effects()
def hypot(*coordinates: _SupportsFloatOrIndex) -> float: no_effects()
def isclose(
    a: _SupportsFloatOrIndex,
    b: _SupportsFloatOrIndex,
    *,
    rel_tol: _SupportsFloatOrIndex = 1e-09,
    abs_tol: _SupportsFloatOrIndex = 0.0,
) -> bool: no_effects()
def isinf(x: _SupportsFloatOrIndex, /) -> bool: no_effects()
def isfinite(x: _SupportsFloatOrIndex, /) -> bool: no_effects()
def isnan(x: _SupportsFloatOrIndex, /) -> bool: no_effects()
def isqrt(n: SupportsIndex, /) -> int: no_effects()
def lcm(*integers: SupportsIndex) -> int: no_effects()
def ldexp(x: _SupportsFloatOrIndex, i: int, /) -> float: no_effects()
def lgamma(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def log(x: _SupportsFloatOrIndex, base: _SupportsFloatOrIndex = ...) -> float: no_effects()
def log10(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def log1p(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def log2(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def modf(x: _SupportsFloatOrIndex, /) -> tuple[float, float]: no_effects()

if sys.version_info >= (3, 12):
    def nextafter(x: _SupportsFloatOrIndex, y: _SupportsFloatOrIndex, /, *, steps: SupportsIndex | None = None) -> float: no_effects()

else:
    def nextafter(x: _SupportsFloatOrIndex, y: _SupportsFloatOrIndex, /) -> float: no_effects()

def perm(n: SupportsIndex, k: SupportsIndex | None = None, /) -> int: no_effects()
def pow(x: _SupportsFloatOrIndex, y: _SupportsFloatOrIndex, /) -> float: no_effects()

_PositiveInteger: TypeAlias = Literal[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25]
_NegativeInteger: TypeAlias = Literal[-1, -2, -3, -4, -5, -6, -7, -8, -9, -10, -11, -12, -13, -14, -15, -16, -17, -18, -19, -20]
_LiteralInteger = _PositiveInteger | _NegativeInteger | Literal[0]  # noqa: Y026  # TODO: Use TypeAlias once mypy bugs are fixed

_MultiplicableT1 = TypeVar("_MultiplicableT1", bound=SupportsMul[Any, Any])
_MultiplicableT2 = TypeVar("_MultiplicableT2", bound=SupportsMul[Any, Any])

class _SupportsProdWithNoDefaultGiven(SupportsMul[Any, Any], SupportsRMul[int, Any], Protocol): ...

_SupportsProdNoDefaultT = TypeVar("_SupportsProdNoDefaultT", bound=_SupportsProdWithNoDefaultGiven)

# This stub is based on the type stub for `builtins.sum`.
# Like `builtins.sum`, it cannot be precisely represented in a type stub
# without introducing many false positives.
# For more details on its limitations and false positives, see #13572.
# Instead, just like `builtins.sum`, we explicitly handle several useful cases.
@overload
def prod(iterable: Iterable[bool | _LiteralInteger], /, *, start: int = 1) -> int: no_effects()  # type: ignore[overload-overlap]
@overload
def prod(iterable: Iterable[_SupportsProdNoDefaultT], /) -> _SupportsProdNoDefaultT | Literal[1]: ...
@overload
def prod(iterable: Iterable[_MultiplicableT1], /, *, start: _MultiplicableT2) -> _MultiplicableT1 | _MultiplicableT2: ...
def radians(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def remainder(x: _SupportsFloatOrIndex, y: _SupportsFloatOrIndex, /) -> float: no_effects()
def sin(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def sinh(x: _SupportsFloatOrIndex, /) -> float: no_effects()

if sys.version_info >= (3, 12):
    def sumprod(p: Iterable[float], q: Iterable[float], /) -> float: no_effects()

def sqrt(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def tan(x: _SupportsFloatOrIndex, /) -> float: no_effects()
def tanh(x: _SupportsFloatOrIndex, /) -> float: no_effects()

# Is different from `_typeshed.SupportsTrunc`, which is not generic
class _SupportsTrunc(Protocol[_T_co]):
    def __trunc__(self) -> _T_co: ...

def trunc(x: _SupportsTrunc[_T], /) -> _T: no_effects()
def ulp(x: _SupportsFloatOrIndex, /) -> float: no_effects()

if sys.version_info >= (3, 13):
    def fma(x: _SupportsFloatOrIndex, y: _SupportsFloatOrIndex, z: _SupportsFloatOrIndex, /) -> float: no_effects()

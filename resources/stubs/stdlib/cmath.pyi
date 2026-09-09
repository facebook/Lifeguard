from typing import Final, SupportsComplex, SupportsFloat, SupportsIndex
from typing_extensions import TypeAlias

e: Final[float]
pi: Final[float]
inf: Final[float]
infj: Final[complex]
nan: Final[float]
nanj: Final[complex]
tau: Final[float]

_C: TypeAlias = SupportsFloat | SupportsComplex | SupportsIndex | complex

def acos(z: _C, /) -> complex: no_effects()
def acosh(z: _C, /) -> complex: no_effects()
def asin(z: _C, /) -> complex: no_effects()
def asinh(z: _C, /) -> complex: no_effects()
def atan(z: _C, /) -> complex: no_effects()
def atanh(z: _C, /) -> complex: no_effects()
def cos(z: _C, /) -> complex: no_effects()
def cosh(z: _C, /) -> complex: no_effects()
def exp(z: _C, /) -> complex: no_effects()
def isclose(a: _C, b: _C, *, rel_tol: SupportsFloat = 1e-09, abs_tol: SupportsFloat = 0.0) -> bool: no_effects()
def isinf(z: _C, /) -> bool: no_effects()
def isnan(z: _C, /) -> bool: no_effects()
def log(x: _C, base: _C = ..., /) -> complex: no_effects()
def log10(z: _C, /) -> complex: no_effects()
def phase(z: _C, /) -> float: no_effects()
def polar(z: _C, /) -> tuple[float, float]: no_effects()
def rect(r: float, phi: float, /) -> complex: no_effects()
def sin(z: _C, /) -> complex: no_effects()
def sinh(z: _C, /) -> complex: no_effects()
def sqrt(z: _C, /) -> complex: no_effects()
def tan(z: _C, /) -> complex: no_effects()
def tanh(z: _C, /) -> complex: no_effects()
def isfinite(z: _C, /) -> bool: no_effects()

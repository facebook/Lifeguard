import _random
import sys
from _typeshed import SupportsLenAndGetItem
from collections.abc import Callable, Iterable, MutableSequence, Sequence, Set as AbstractSet
from fractions import Fraction
from typing import Any, ClassVar, NoReturn, TypeVar

__all__ = [
    "Random",
    "seed",
    "random",
    "uniform",
    "randint",
    "choice",
    "sample",
    "randrange",
    "shuffle",
    "normalvariate",
    "lognormvariate",
    "expovariate",
    "vonmisesvariate",
    "gammavariate",
    "triangular",
    "gauss",
    "betavariate",
    "paretovariate",
    "weibullvariate",
    "getstate",
    "setstate",
    "getrandbits",
    "choices",
    "SystemRandom",
    "randbytes",
]

if sys.version_info >= (3, 12):
    __all__ += ["binomialvariate"]

_T = TypeVar("_T")

class Random(_random.Random):
    VERSION: ClassVar[int]
    def __init__(self, x: int | float | str | bytes | bytearray | None = None) -> None: no_effects()  # noqa: Y041
    # Using other `seed` types is deprecated since 3.9 and removed in 3.11
    # Ignore Y041, since random.seed doesn't treat int like a float subtype. Having an explicit
    # int better documents conventional usage of random.seed.
    def seed(self, a: int | float | str | bytes | bytearray | None = None, version: int = 2) -> None: mutation()  # type: ignore[override]  # noqa: Y041
    def getstate(self) -> tuple[Any, ...]: no_effects()
    def setstate(self, state: tuple[Any, ...]) -> None: mutation()
    def randrange(self, start: int, stop: int | None = None, step: int = 1) -> int: no_effects()
    def randint(self, a: int, b: int) -> int: no_effects()
    def randbytes(self, n: int) -> bytes: no_effects()
    def choice(self, seq: SupportsLenAndGetItem[_T]) -> _T: no_effects()
    def choices(
        self,
        population: SupportsLenAndGetItem[_T],
        weights: Sequence[float | Fraction] | None = None,
        *,
        cum_weights: Sequence[float | Fraction] | None = None,
        k: int = 1,
    ) -> list[_T]: no_effects()
    if sys.version_info >= (3, 11):
        def shuffle(self, x: MutableSequence[Any]) -> None: mutation()
    else:
        def shuffle(self, x: MutableSequence[Any], random: Callable[[], float] | None = None) -> None: mutation()
    if sys.version_info >= (3, 11):
        def sample(self, population: Sequence[_T], k: int, *, counts: Iterable[int] | None = None) -> list[_T]: no_effects()
    else:
        def sample(
            self, population: Sequence[_T] | AbstractSet[_T], k: int, *, counts: Iterable[int] | None = None
        ) -> list[_T]: no_effects()

    def uniform(self, a: float, b: float) -> float: no_effects()
    def triangular(self, low: float = 0.0, high: float = 1.0, mode: float | None = None) -> float: no_effects()
    if sys.version_info >= (3, 12):
        def binomialvariate(self, n: int = 1, p: float = 0.5) -> int: no_effects()

    def betavariate(self, alpha: float, beta: float) -> float: no_effects()
    if sys.version_info >= (3, 12):
        def expovariate(self, lambd: float = 1.0) -> float: no_effects()
    else:
        def expovariate(self, lambd: float) -> float: no_effects()

    def gammavariate(self, alpha: float, beta: float) -> float: no_effects()
    if sys.version_info >= (3, 11):
        def gauss(self, mu: float = 0.0, sigma: float = 1.0) -> float: no_effects()
        def normalvariate(self, mu: float = 0.0, sigma: float = 1.0) -> float: no_effects()
    else:
        def gauss(self, mu: float, sigma: float) -> float: no_effects()
        def normalvariate(self, mu: float, sigma: float) -> float: no_effects()

    def lognormvariate(self, mu: float, sigma: float) -> float: no_effects()
    def vonmisesvariate(self, mu: float, kappa: float) -> float: no_effects()
    def paretovariate(self, alpha: float) -> float: no_effects()
    def weibullvariate(self, alpha: float, beta: float) -> float: no_effects()

# SystemRandom is not implemented for all OS's; good on Windows & Linux
class SystemRandom(Random):
    def getrandbits(self, k: int) -> int: no_effects()  # k can be passed by keyword
    def getstate(self, *args: Any, **kwds: Any) -> NoReturn: no_effects()
    def setstate(self, *args: Any, **kwds: Any) -> NoReturn: mutation()

_inst: Random
seed = _inst.seed
random = _inst.random
uniform = _inst.uniform
triangular = _inst.triangular
randint = _inst.randint
choice = _inst.choice
randrange = _inst.randrange
sample = _inst.sample
shuffle = _inst.shuffle
choices = _inst.choices
normalvariate = _inst.normalvariate
lognormvariate = _inst.lognormvariate
expovariate = _inst.expovariate
vonmisesvariate = _inst.vonmisesvariate
gammavariate = _inst.gammavariate
gauss = _inst.gauss
if sys.version_info >= (3, 12):
    binomialvariate = _inst.binomialvariate
betavariate = _inst.betavariate
paretovariate = _inst.paretovariate
weibullvariate = _inst.weibullvariate
getstate = _inst.getstate
setstate = _inst.setstate
getrandbits = _inst.getrandbits
randbytes = _inst.randbytes

# A stub to use in tests. Add any features we need to test in here.

def foo():
    ...  # will have unknown-effects


def bar():
    no_effects()


def baz():
    unsafe()


# One overload set per annotation kind, each with the annotation on the first
# overload and bare bodies after it, so the bare ones cannot change the verdict.
@overload
def all_bare_overloads(x: int):
    ...


@overload
def all_bare_overloads(x: str):
    ...


@overload
def all_bare_overloads(x: float):
    ...


@overload
def no_effects_first_overload(x: int):
    no_effects()


@overload
def no_effects_first_overload(x: str):
    ...


@overload
def no_effects_first_overload(x: float):
    ...


@overload
def unsafe_first_overload(x: int):
    unsafe()


@overload
def unsafe_first_overload(x: str):
    ...


@overload
def unsafe_first_overload(x: float):
    ...


@overload
def mutation_first_overload(x: int):
    mutation()


@overload
def mutation_first_overload(x: str):
    ...


@overload
def mutation_first_overload(x: float):
    ...


class A:
    def f(x):
        no_effects()

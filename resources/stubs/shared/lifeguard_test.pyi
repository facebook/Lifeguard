# A stub to use in tests. Add any features we need to test in here.

def foo():
    ...  # will have unknown-effects


def bar():
    no_effects()


def baz():
    unsafe()


class A:
    def f(x):
        no_effects()

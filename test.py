#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.


"""
Test that everything works well

Runs format, lint, test and build. Each step has two implementations: `cargo`
for open-source checkouts and `buck` for internal ones. The mode is detected
automatically; pass `--mode` to force one.

The cargo path mirrors .github/workflows/lifeguard.yml, except that it formats
rather than checking formatting, so a failing `cargo fmt -- --check` in CI
becomes a fixed working copy here.
"""

from __future__ import annotations

import abc
import argparse
import dataclasses
import os
import platform
import shlex
import shutil
import signal
import subprocess
import sys
import time
from collections.abc import Generator, Iterable
from contextlib import contextmanager
from enum import Enum
from pathlib import Path
from typing import final

# argparse.BooleanOptionalAction, used by every flag below. Only 3.8 reaches the
# check in require_python_version: the imports above already need 3.8 for
# `typing.final`, and nothing can run ahead of them.
MIN_PYTHON: tuple[int, int] = (3, 9)


def script_path() -> Path:
    return Path(__file__).parent.absolute()


def cargo_build_script() -> Path:
    return script_path() / "scripts" / "test_cargo_build.sh"


class Colors(Enum):
    # Copied from https://stackoverflow.com/questions/287871/how-to-print-colored-text-to-the-terminal
    HEADER = "\033[95m"
    OKBLUE = "\033[94m"
    OKCYAN = "\033[96m"
    OKGREEN = "\033[92m"
    WARNING = "\033[93m"
    FAIL = "\033[91m"
    ENDC = "\033[0m"
    BOLD = "\033[1m"
    UNDERLINE = "\033[4m"


@dataclasses.dataclass(frozen=True)
class TestFlags:
    run_fmt: bool
    run_lint: bool
    run_test: bool
    run_build: bool


def _eprintln(msg: str) -> None:
    print(msg, file=sys.stderr)


def print_step(msg: str) -> None:
    _eprintln(Colors.OKCYAN.value + msg + Colors.ENDC.value)


def print_running(msg: str) -> None:
    _eprintln(Colors.OKGREEN.value + "Running " + msg + Colors.ENDC.value)


def require_tools(tools: Iterable[str], hint: str) -> None:
    missing = [tool for tool in tools if shutil.which(tool) is None]
    if missing:
        _eprintln(
            Colors.FAIL.value
            + f"This host has no {' and '.join(missing)}. {hint}"
            + Colors.ENDC.value
        )
        sys.exit(1)


@contextmanager
def timing() -> Generator[None, None, None]:
    start = time.time()
    yield
    duration = time.time() - start
    _eprintln(f"Finished in {duration:.2f} seconds.")


def run(
    args: Iterable[str],
    capture_output: bool = False,
) -> subprocess.CompletedProcess:
    """
    Run a command in a new process, exiting non-zero if it fails.

    With capture_output, stdout/stderr are recorded on the CompletedProcess and
    only replayed on failure; otherwise they go straight to the console.
    """
    argv = tuple(args)
    print_running(shlex.join(argv))
    # On CI stderr gets out of order with stdout unless both are flushed first.
    sys.stdout.flush()
    sys.stderr.flush()
    try:
        return subprocess.run(
            argv,
            stdout=subprocess.PIPE if capture_output else sys.stdout,
            stderr=subprocess.PIPE if capture_output else sys.stderr,
            check=True,
            encoding="utf-8",
        )
    except subprocess.CalledProcessError as e:
        if capture_output:
            print(e.stdout, file=sys.stdout)
            print(e.stderr, file=sys.stderr)
        sys.exit(1)


class Executor(abc.ABC):
    @abc.abstractmethod
    def rustfmt(self) -> None:
        raise NotImplementedError()

    @abc.abstractmethod
    def clippy(self) -> None:
        raise NotImplementedError()

    @abc.abstractmethod
    def test(self) -> None:
        raise NotImplementedError()

    @abc.abstractmethod
    def build(self) -> None:
        raise NotImplementedError()


@final
class CargoExecutor(Executor):
    """Open-source path. Needs only a rustup toolchain and the pyrefly submodule."""

    def __init__(self, release: bool) -> None:
        require_tools(["cargo"], "Install a rustup toolchain: https://rustup.rs.")
        self._flags: list[str] = ["--release"] if release else []

    def rustfmt(self) -> None:
        # rust-toolchain.toml pins the nightly .rustfmt.toml's options need and
        # declares the rustfmt and clippy components, so bare `cargo` is already
        # the right toolchain under rustup.
        run(["cargo", "fmt"])

    def clippy(self) -> None:
        run(["cargo", "clippy", *self._flags])

    def test(self) -> None:
        run(["cargo", "test", *self._flags])

    def build(self) -> None:
        run(["cargo", "build", "--bin", "lifeguard", *self._flags])


def skip_on_sandcastle(step: str) -> bool:
    """CI schedules the Buck tests and the lifeguard-oss-linux job itself."""
    if "SANDCASTLE_NONCE" in os.environ:
        _eprintln(f"Skipping {step} on CI: already scheduled there.")
        return True
    return False


@final
class BuckExecutor(Executor):
    """Internal path. `--release` does not apply; use `@fbcode//mode/opt` instead."""

    def __init__(self) -> None:
        require_tools(
            ["arc", "buck2"],
            "If this is an open-source checkout, rerun with --mode cargo.",
        )

    def rustfmt(self) -> None:
        run(["arc", "f"])

    def clippy(self) -> None:
        run(
            [
                "arc",
                "rust-clippy",
                "...",
                "--reuse-current-config",
            ]
        )

    def test(self) -> None:
        if skip_on_sandcastle("tests"):
            return

        res = run(
            [
                "buck2",
                "uquery",
                "kind('rust_test|rust_library', ...)",
                "--reuse-current-config",
            ],
            capture_output=True,
        )
        tests = [line.strip() for line in res.stdout.splitlines()]
        run(
            ["buck2", "test", "--reuse-current-config"]
            + tests
            + ["--", "--run-disabled"]
        )

    def build(self) -> None:
        # Buck is the internal build of record, so what is worth checking here is
        # that the open-source cargo build still works.
        if skip_on_sandcastle("the OSS cargo build"):
            return

        run(["bash", str(cargo_build_script())])


def get_executor(mode: str, release: bool) -> Executor:
    if mode == "auto":
        # ShipIt strips scripts/ from the open-source export but keeps BUCK, so
        # this script's absence -- not the BUCK file -- is what marks the export.
        mode = "buck" if cargo_build_script().is_file() else "cargo"
    _eprintln(f"Mode: {mode}")
    return BuckExecutor() if mode == "buck" else CargoExecutor(release)


def run_tests(executor: Executor, test_flags: TestFlags) -> None:
    steps = (
        (test_flags.run_fmt, "Formatting", executor.rustfmt),
        (test_flags.run_lint, "Linting", executor.clippy),
        (test_flags.run_test, "Tests", executor.test),
        (test_flags.run_build, "Build", executor.build),
    )
    for enabled, label, step in steps:
        if enabled:
            print_step(label)
            with timing():
                step()


def main(mode: str, release: bool, test_flags: TestFlags) -> None:
    _eprintln(f"Python executable: {sys.executable}")
    _eprintln(f"Python version_info: {sys.version_info}")

    os.chdir(script_path())
    run_tests(get_executor(mode, release), test_flags)


def require_python_version() -> None:
    if sys.version_info < MIN_PYTHON:
        wanted = ".".join(str(part) for part in MIN_PYTHON)
        _eprintln(
            Colors.FAIL.value
            + f"test.py needs Python {wanted} or newer, but {sys.executable} is "
            + f"{platform.python_version()}."
            + Colors.ENDC.value
        )
        sys.exit(1)


def invoke_main() -> None:
    require_python_version()
    parser = argparse.ArgumentParser(description="Lifeguard test script")
    parser.add_argument(
        "--mode",
        "-m",
        choices=["buck", "cargo", "auto"],
        default="auto",
        help="Build the project with buck or cargo. Default is auto-detect.",
    )
    parser.add_argument(
        "--fmt",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Whether to run code formatting or not",
    )
    parser.add_argument(
        "--lint",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Whether to run code linting or not",
    )
    parser.add_argument(
        "--test",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Whether to run testing or not",
    )
    parser.add_argument(
        "--build",
        action=argparse.BooleanOptionalAction,
        default=True,
        help=(
            "Whether to build the lifeguard binary or not. In Buck mode this is "
            "the whole open-source cargo check, cargo tests included, so it is "
            "the step to drop for a quick run."
        ),
    )
    parser.add_argument(
        "--release",
        action=argparse.BooleanOptionalAction,
        default=True,
        help=(
            "Cargo mode only: build and test in release, as GitHub CI does. "
            "Pass --no-release for a faster run."
        ),
    )
    args = parser.parse_args()
    main(
        args.mode,
        args.release,
        TestFlags(
            run_fmt=args.fmt,
            run_lint=args.lint,
            run_test=args.test,
            run_build=args.build,
        ),
    )


if __name__ == "__main__":
    try:
        invoke_main()
    except KeyboardInterrupt:
        # Re-raise SIGINT with the default handler so the OS sets exit code 130
        # (128 + SIGINT), correctly signalling an interrupted process to shells
        # and CI systems, rather than exiting with a generic error code.
        signal.signal(signal.SIGINT, signal.SIG_DFL)
        os.kill(os.getpid(), signal.SIGINT)

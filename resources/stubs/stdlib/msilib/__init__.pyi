import sys
from collections.abc import Container, Iterable, Sequence
from types import ModuleType
from typing import Any, Literal

if sys.platform == "win32":
    from _msi import *
    from _msi import _Database

    AMD64: bool
    Win64: bool

    datasizemask: Literal[0x00FF]
    type_valid: Literal[0x0100]
    type_localizable: Literal[0x0200]
    typemask: Literal[0x0C00]
    type_long: Literal[0x0000]
    type_short: Literal[0x0400]
    type_string: Literal[0x0C00]
    type_binary: Literal[0x0800]
    type_nullable: Literal[0x1000]
    type_key: Literal[0x2000]
    knownbits: Literal[0x3FFF]

    class Table:
        name: str
        fields: list[tuple[int, str, int]]
        def __init__(self, name: str) -> None: no_effects()
        def add_field(self, index: int, name: str, type: int) -> None: mutation()
        def sql(self) -> str: no_effects()
        def create(self, db: _Database) -> None: unsafe()

    class _Unspecified: ...

    def change_sequence(
        seq: Sequence[tuple[str, str | None, int]],
        action: str,
        seqno: int | type[_Unspecified] = ...,
        cond: str | type[_Unspecified] = ...,
    ) -> None: unsafe()
    def add_data(db: _Database, table: str, values: Iterable[tuple[Any, ...]]) -> None: unsafe()
    def add_stream(db: _Database, name: str, path: str) -> None: unsafe()
    def init_database(
        name: str, schema: ModuleType, ProductName: str, ProductCode: str, ProductVersion: str, Manufacturer: str
    ) -> _Database: unsafe()
    def add_tables(db: _Database, module: ModuleType) -> None: unsafe()
    def make_id(str: str) -> str: no_effects()
    def gen_uuid() -> str: no_effects()

    class CAB:
        name: str
        files: list[tuple[str, str]]
        filenames: set[str]
        index: int
        def __init__(self, name: str) -> None: no_effects()
        def gen_id(self, file: str) -> str: no_effects()
        def append(self, full: str, file: str, logical: str) -> tuple[int, str]: mutation()
        def commit(self, db: _Database) -> None: unsafe()

    _directories: set[str]

    class Directory:
        db: _Database
        cab: CAB
        basedir: str
        physical: str
        logical: str
        component: str | None
        short_names: set[str]
        ids: set[str]
        keyfiles: dict[str, str]
        componentflags: int | None
        absolute: str
        def __init__(
            self,
            db: _Database,
            cab: CAB,
            basedir: str,
            physical: str,
            _logical: str,
            default: str,
            componentflags: int | None = None,
        ) -> None: unsafe()
        def start_component(
            self,
            component: str | None = None,
            feature: Feature | None = None,
            flags: int | None = None,
            keyfile: str | None = None,
            uuid: str | None = None,
        ) -> None: unsafe()
        def make_short(self, file: str) -> str: no_effects()
        def add_file(self, file: str, src: str | None = None, version: str | None = None, language: str | None = None) -> str: unsafe()
        def glob(self, pattern: str, exclude: Container[str] | None = None) -> list[str]: unsafe()
        def remove_pyc(self) -> None: unsafe()

    class Binary:
        name: str
        def __init__(self, fname: str) -> None: no_effects()

    class Feature:
        id: str
        def __init__(
            self,
            db: _Database,
            id: str,
            title: str,
            desc: str,
            display: int,
            level: int = 1,
            parent: Feature | None = None,
            directory: str | None = None,
            attributes: int = 0,
        ) -> None: unsafe()
        def set_current(self) -> None: unsafe()

    class Control:
        dlg: Dialog
        name: str
        def __init__(self, dlg: Dialog, name: str) -> None: no_effects()
        def event(self, event: str, argument: str, condition: str = "1", ordering: int | None = None) -> None: unsafe()
        def mapping(self, event: str, attribute: str) -> None: unsafe()
        def condition(self, action: str, condition: str) -> None: unsafe()

    class RadioButtonGroup(Control):
        property: str
        index: int
        def __init__(self, dlg: Dialog, name: str, property: str) -> None: no_effects()
        def add(self, name: str, x: int, y: int, w: int, h: int, text: str, value: str | None = None) -> None: unsafe()

    class Dialog:
        db: _Database
        name: str
        x: int
        y: int
        w: int
        h: int
        def __init__(
            self,
            db: _Database,
            name: str,
            x: int,
            y: int,
            w: int,
            h: int,
            attr: int,
            title: str,
            first: str,
            default: str,
            cancel: str,
        ) -> None: unsafe()
        def control(
            self,
            name: str,
            type: str,
            x: int,
            y: int,
            w: int,
            h: int,
            attr: int,
            prop: str | None,
            text: str | None,
            next: str | None,
            help: str | None,
        ) -> Control: unsafe()
        def text(self, name: str, x: int, y: int, w: int, h: int, attr: int, text: str | None) -> Control: unsafe()
        def bitmap(self, name: str, x: int, y: int, w: int, h: int, text: str | None) -> Control: unsafe()
        def line(self, name: str, x: int, y: int, w: int, h: int) -> Control: unsafe()
        def pushbutton(
            self, name: str, x: int, y: int, w: int, h: int, attr: int, text: str | None, next: str | None
        ) -> Control: unsafe()
        def radiogroup(
            self, name: str, x: int, y: int, w: int, h: int, attr: int, prop: str | None, text: str | None, next: str | None
        ) -> RadioButtonGroup: unsafe()
        def checkbox(
            self, name: str, x: int, y: int, w: int, h: int, attr: int, prop: str | None, text: str | None, next: str | None
        ) -> Control: unsafe()

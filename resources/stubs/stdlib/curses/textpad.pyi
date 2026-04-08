from _curses import window
from collections.abc import Callable

def rectangle(win: window, uly: int, ulx: int, lry: int, lrx: int) -> None: unsafe()

class Textbox:
    stripspaces: bool
    def __init__(self, win: window, insert_mode: bool = False) -> None: unsafe()
    def edit(self, validate: Callable[[int], int] | None = None) -> str: unsafe()
    def do_command(self, ch: str | int) -> None: unsafe()
    def gather(self) -> str: unsafe()

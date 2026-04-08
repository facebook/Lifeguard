from _markupbase import ParserBase
from re import Pattern

__all__ = ["HTMLParser"]

class HTMLParser(ParserBase):
    def __init__(self, *, convert_charrefs: bool = True) -> None: no_effects()
    def feed(self, data: str) -> None: mutation()
    def close(self) -> None: mutation()
    def get_starttag_text(self) -> str | None: no_effects()
    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None: no_effects()
    def handle_endtag(self, tag: str) -> None: no_effects()
    def handle_startendtag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None: no_effects()
    def handle_data(self, data: str) -> None: no_effects()
    def handle_entityref(self, name: str) -> None: no_effects()
    def handle_charref(self, name: str) -> None: no_effects()
    def handle_comment(self, data: str) -> None: no_effects()
    def handle_decl(self, decl: str) -> None: no_effects()
    def handle_pi(self, data: str) -> None: no_effects()
    CDATA_CONTENT_ELEMENTS: tuple[str, ...]
    def check_for_whole_start_tag(self, i: int) -> int: no_effects()  # undocumented
    def clear_cdata_mode(self) -> None: mutation()  # undocumented
    def goahead(self, end: bool) -> None: mutation()  # undocumented
    def parse_bogus_comment(self, i: int, report: bool = True) -> int: no_effects()  # undocumented
    def parse_endtag(self, i: int) -> int: no_effects()  # undocumented
    def parse_html_declaration(self, i: int) -> int: no_effects()  # undocumented
    def parse_pi(self, i: int) -> int: no_effects()  # undocumented
    def parse_starttag(self, i: int) -> int: no_effects()  # undocumented
    def set_cdata_mode(self, elem: str) -> None: mutation()  # undocumented
    rawdata: str  # undocumented
    cdata_elem: str | None  # undocumented
    convert_charrefs: bool  # undocumented
    interesting: Pattern[str]  # undocumented
    lasttag: str  # undocumented

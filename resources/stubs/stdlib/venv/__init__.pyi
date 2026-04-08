import logging
import sys
from _typeshed import StrOrBytesPath
from collections.abc import Iterable, Sequence
from types import SimpleNamespace

logger: logging.Logger

CORE_VENV_DEPS: tuple[str, ...]

class EnvBuilder:
    system_site_packages: bool
    clear: bool
    symlinks: bool
    upgrade: bool
    with_pip: bool
    prompt: str | None

    if sys.version_info >= (3, 13):
        def __init__(
            self,
            system_site_packages: bool = False,
            clear: bool = False,
            symlinks: bool = False,
            upgrade: bool = False,
            with_pip: bool = False,
            prompt: str | None = None,
            upgrade_deps: bool = False,
            *,
            scm_ignore_files: Iterable[str] = ...,
        ) -> None: no_effects()
    else:
        def __init__(
            self,
            system_site_packages: bool = False,
            clear: bool = False,
            symlinks: bool = False,
            upgrade: bool = False,
            with_pip: bool = False,
            prompt: str | None = None,
            upgrade_deps: bool = False,
        ) -> None: no_effects()

    def create(self, env_dir: StrOrBytesPath) -> None: unsafe()
    def clear_directory(self, path: StrOrBytesPath) -> None: unsafe()  # undocumented
    def ensure_directories(self, env_dir: StrOrBytesPath) -> SimpleNamespace: unsafe()
    def create_configuration(self, context: SimpleNamespace) -> None: unsafe()
    def symlink_or_copy(
        self, src: StrOrBytesPath, dst: StrOrBytesPath, relative_symlinks_ok: bool = False
    ) -> None: unsafe()  # undocumented
    def setup_python(self, context: SimpleNamespace) -> None: unsafe()
    def _setup_pip(self, context: SimpleNamespace) -> None: unsafe()  # undocumented
    def setup_scripts(self, context: SimpleNamespace) -> None: unsafe()
    def post_setup(self, context: SimpleNamespace) -> None: no_effects()
    def replace_variables(self, text: str, context: SimpleNamespace) -> str: no_effects()  # undocumented
    def install_scripts(self, context: SimpleNamespace, path: str) -> None: unsafe()
    def upgrade_dependencies(self, context: SimpleNamespace) -> None: unsafe()
    if sys.version_info >= (3, 13):
        def create_git_ignore_file(self, context: SimpleNamespace) -> None: unsafe()

if sys.version_info >= (3, 13):
    def create(
        env_dir: StrOrBytesPath,
        system_site_packages: bool = False,
        clear: bool = False,
        symlinks: bool = False,
        with_pip: bool = False,
        prompt: str | None = None,
        upgrade_deps: bool = False,
        *,
        scm_ignore_files: Iterable[str] = ...,
    ) -> None: unsafe()

else:
    def create(
        env_dir: StrOrBytesPath,
        system_site_packages: bool = False,
        clear: bool = False,
        symlinks: bool = False,
        with_pip: bool = False,
        prompt: str | None = None,
        upgrade_deps: bool = False,
    ) -> None: unsafe()

def main(args: Sequence[str] | None = None) -> None: unsafe()

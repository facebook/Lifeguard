import sys
from collections.abc import Callable, Generator, Iterable, Mapping, MutableMapping, MutableSequence
from multiprocessing.connection import Connection
from multiprocessing.context import BaseContext, Process
from multiprocessing.queues import Queue, SimpleQueue
from threading import Lock, Semaphore, Thread
from types import TracebackType
from typing import Any, Generic, TypeVar, overload
from typing_extensions import TypeVarTuple, Unpack
from weakref import ref

from ._base import BrokenExecutor, Executor, Future

_T = TypeVar("_T")
_Ts = TypeVarTuple("_Ts")

_threads_wakeups: MutableMapping[Any, Any]
_global_shutdown: bool

class _ThreadWakeup:
    _closed: bool
    # Any: Unused send and recv methods
    _reader: Connection[Any, Any]
    _writer: Connection[Any, Any]
    def close(self) -> None: unsafe()
    def wakeup(self) -> None: unsafe()
    def clear(self) -> None: mutation()

def _python_exit() -> None: unsafe()

EXTRA_QUEUED_CALLS: int

_MAX_WINDOWS_WORKERS: int

class _RemoteTraceback(Exception):
    tb: str
    def __init__(self, tb: TracebackType) -> None: no_effects()

class _ExceptionWithTraceback:
    exc: BaseException
    tb: TracebackType
    def __init__(self, exc: BaseException, tb: TracebackType) -> None: no_effects()
    def __reduce__(self) -> str | tuple[Any, ...]: no_effects()

def _rebuild_exc(exc: Exception, tb: str) -> Exception: no_effects()

class _WorkItem(Generic[_T]):
    future: Future[_T]
    fn: Callable[..., _T]
    args: Iterable[Any]
    kwargs: Mapping[str, Any]
    def __init__(self, future: Future[_T], fn: Callable[..., _T], args: Iterable[Any], kwargs: Mapping[str, Any]) -> None: no_effects()

class _ResultItem:
    work_id: int
    exception: Exception
    result: Any
    if sys.version_info >= (3, 11):
        exit_pid: int | None
        def __init__(
            self, work_id: int, exception: Exception | None = None, result: Any | None = None, exit_pid: int | None = None
        ) -> None: no_effects()
    else:
        def __init__(self, work_id: int, exception: Exception | None = None, result: Any | None = None) -> None: no_effects()

class _CallItem:
    work_id: int
    fn: Callable[..., Any]
    args: Iterable[Any]
    kwargs: Mapping[str, Any]
    def __init__(self, work_id: int, fn: Callable[..., Any], args: Iterable[Any], kwargs: Mapping[str, Any]) -> None: no_effects()

class _SafeQueue(Queue[Future[Any]]):
    pending_work_items: dict[int, _WorkItem[Any]]
    if sys.version_info < (3, 12):
        shutdown_lock: Lock
    thread_wakeup: _ThreadWakeup
    if sys.version_info >= (3, 12):
        def __init__(
            self,
            max_size: int | None = 0,
            *,
            ctx: BaseContext,
            pending_work_items: dict[int, _WorkItem[Any]],
            thread_wakeup: _ThreadWakeup,
        ) -> None: no_effects()
    else:
        def __init__(
            self,
            max_size: int | None = 0,
            *,
            ctx: BaseContext,
            pending_work_items: dict[int, _WorkItem[Any]],
            shutdown_lock: Lock,
            thread_wakeup: _ThreadWakeup,
        ) -> None: no_effects()

    def _on_queue_feeder_error(self, e: Exception, obj: _CallItem) -> None: unsafe()

def _get_chunks(*iterables: Any, chunksize: int) -> Generator[tuple[Any, ...], None, None]: no_effects()
def _process_chunk(fn: Callable[..., _T], chunk: Iterable[tuple[Any, ...]]) -> list[_T]: unsafe()

if sys.version_info >= (3, 11):
    def _sendback_result(
        result_queue: SimpleQueue[_WorkItem[Any]],
        work_id: int,
        result: Any | None = None,
        exception: Exception | None = None,
        exit_pid: int | None = None,
    ) -> None: unsafe()

else:
    def _sendback_result(
        result_queue: SimpleQueue[_WorkItem[Any]], work_id: int, result: Any | None = None, exception: Exception | None = None
    ) -> None: unsafe()

if sys.version_info >= (3, 11):
    def _process_worker(
        call_queue: Queue[_CallItem],
        result_queue: SimpleQueue[_ResultItem],
        initializer: Callable[[Unpack[_Ts]], object] | None,
        initargs: tuple[Unpack[_Ts]],
        max_tasks: int | None = None,
    ) -> None: unsafe()

else:
    def _process_worker(
        call_queue: Queue[_CallItem],
        result_queue: SimpleQueue[_ResultItem],
        initializer: Callable[[Unpack[_Ts]], object] | None,
        initargs: tuple[Unpack[_Ts]],
    ) -> None: unsafe()

class _ExecutorManagerThread(Thread):
    thread_wakeup: _ThreadWakeup
    shutdown_lock: Lock
    executor_reference: ref[Any]
    processes: MutableMapping[int, Process]
    call_queue: Queue[_CallItem]
    result_queue: SimpleQueue[_ResultItem]
    work_ids_queue: Queue[int]
    pending_work_items: dict[int, _WorkItem[Any]]
    def __init__(self, executor: ProcessPoolExecutor) -> None: no_effects()
    def run(self) -> None: unsafe()
    def add_call_item_to_queue(self) -> None: unsafe()
    def wait_result_broken_or_wakeup(self) -> tuple[Any, bool, str]: unsafe()
    def process_result_item(self, result_item: int | _ResultItem) -> None: unsafe()
    def is_shutting_down(self) -> bool: no_effects()
    def terminate_broken(self, cause: str) -> None: unsafe()
    def flag_executor_shutting_down(self) -> None: mutation()
    def shutdown_workers(self) -> None: unsafe()
    def join_executor_internals(self) -> None: unsafe()
    def get_n_children_alive(self) -> int: no_effects()

_system_limits_checked: bool
_system_limited: bool | None

def _check_system_limits() -> None: unsafe()
def _chain_from_iterable_of_lists(iterable: Iterable[MutableSequence[Any]]) -> Any: no_effects()

class BrokenProcessPool(BrokenExecutor): ...

class ProcessPoolExecutor(Executor):
    _mp_context: BaseContext | None
    _initializer: Callable[..., None] | None
    _initargs: tuple[Any, ...]
    _executor_manager_thread: _ThreadWakeup
    _processes: MutableMapping[int, Process]
    _shutdown_thread: bool
    _shutdown_lock: Lock
    _idle_worker_semaphore: Semaphore
    _broken: bool
    _queue_count: int
    _pending_work_items: dict[int, _WorkItem[Any]]
    _cancel_pending_futures: bool
    _executor_manager_thread_wakeup: _ThreadWakeup
    _result_queue: SimpleQueue[Any]
    _work_ids: Queue[Any]
    if sys.version_info >= (3, 11):
        @overload
        def __init__(
            self,
            max_workers: int | None = None,
            mp_context: BaseContext | None = None,
            initializer: Callable[[], object] | None = None,
            initargs: tuple[()] = (),
            *,
            max_tasks_per_child: int | None = None,
        ) -> None: no_effects()
        @overload
        def __init__(
            self,
            max_workers: int | None = None,
            mp_context: BaseContext | None = None,
            *,
            initializer: Callable[[Unpack[_Ts]], object],
            initargs: tuple[Unpack[_Ts]],
            max_tasks_per_child: int | None = None,
        ) -> None: ...
        @overload
        def __init__(
            self,
            max_workers: int | None,
            mp_context: BaseContext | None,
            initializer: Callable[[Unpack[_Ts]], object],
            initargs: tuple[Unpack[_Ts]],
            *,
            max_tasks_per_child: int | None = None,
        ) -> None: ...
    else:
        @overload
        def __init__(
            self,
            max_workers: int | None = None,
            mp_context: BaseContext | None = None,
            initializer: Callable[[], object] | None = None,
            initargs: tuple[()] = (),
        ) -> None: no_effects()
        @overload
        def __init__(
            self,
            max_workers: int | None = None,
            mp_context: BaseContext | None = None,
            *,
            initializer: Callable[[Unpack[_Ts]], object],
            initargs: tuple[Unpack[_Ts]],
        ) -> None: ...
        @overload
        def __init__(
            self,
            max_workers: int | None,
            mp_context: BaseContext | None,
            initializer: Callable[[Unpack[_Ts]], object],
            initargs: tuple[Unpack[_Ts]],
        ) -> None: ...

    def _start_executor_manager_thread(self) -> None: unsafe()
    def _adjust_process_count(self) -> None: unsafe()

    if sys.version_info >= (3, 14):
        def kill_workers(self) -> None: unsafe()
        def terminate_workers(self) -> None: unsafe()

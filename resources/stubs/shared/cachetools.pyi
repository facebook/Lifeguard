# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.


# ctors only set attributes, decorators return update_wrapper closures

from collections.abc import Callable, MutableMapping
from typing import Any

class Cache(MutableMapping[Any, Any]):
    def __init__(self, maxsize: float, getsizeof: Callable[[Any], float] | None = None) -> None: no_effects()

class FIFOCache(Cache):
    def __init__(self, maxsize: float, getsizeof: Callable[[Any], float] | None = None) -> None: no_effects()

class LFUCache(Cache):
    def __init__(self, maxsize: float, getsizeof: Callable[[Any], float] | None = None) -> None: no_effects()

class LRUCache(Cache):
    def __init__(self, maxsize: float, getsizeof: Callable[[Any], float] | None = None) -> None: no_effects()

class RRCache(Cache):
    def __init__(
        self,
        maxsize: float,
        choice: Callable[[Any], Any] = ...,
        getsizeof: Callable[[Any], float] | None = None,
    ) -> None: no_effects()

class TTLCache(Cache):
    def __init__(
        self,
        maxsize: float,
        ttl: float,
        timer: Callable[[], float] = ...,
        getsizeof: Callable[[Any], float] | None = None,
    ) -> None: no_effects()

class TLRUCache(Cache):
    def __init__(
        self,
        maxsize: float,
        ttu: Callable[[Any, Any, float], float],
        timer: Callable[[], float] = ...,
        getsizeof: Callable[[Any], float] | None = None,
    ) -> None: no_effects()

def cached(
    cache: MutableMapping[Any, Any] | None,
    key: Callable[..., Any] = ...,
    lock: Any = None,
    condition: Any = None,
    info: bool = False,
) -> Callable[[Callable[..., Any]], Callable[..., Any]]: no_effects()

def cachedmethod(
    cache: Callable[[Any], MutableMapping[Any, Any] | None],
    key: Callable[..., Any] = ...,
    lock: Callable[[Any], Any] | None = None,
    condition: Callable[[Any], Any] | None = None,
) -> Callable[[Callable[..., Any]], Callable[..., Any]]: no_effects()

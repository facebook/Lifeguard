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

# trace_point logs the trace point at call time; decoration has no effects.
def trace_point(name, attributes=None): no_effects()

# get_trace_id delegates to _backend.get_trace_id_impl(), a pure read of the
# ambient trace context that returns the id (or None). No mutation or I/O.
def get_trace_id(): no_effects()

# trace_function_block reads func.__qualname__ and returns a functools.wraps
# wrapper; all block-scope tracing is deferred to call time.
def trace_function_block(func): no_effects()

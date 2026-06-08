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

# All of these are pure-wrapper decorators: at decoration time they only
# validate arguments and return an `argmap` wrapper, with no module-scope side
# effects. The actual logic runs lazily when the decorated function is called.

def not_implemented_for(*graph_types): no_effects()

def open_file(path_arg, mode=...): no_effects()

def nodes_or_number(which_args): no_effects()

def np_random_state(random_state_argument): no_effects()

def py_random_state(random_state_argument): no_effects()

class argmap:
    def __init__(self, func, *args, **kwargs): no_effects()

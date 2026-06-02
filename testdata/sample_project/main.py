# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.

# pyre-unsafe
from safer_lazy_imports.lifeguard.testdata.sample_project.importer import greet
from safer_lazy_imports.lifeguard.testdata.sample_project.pkg import sub
from safer_lazy_imports.lifeguard.testdata.sample_project.safe_module import Config

config_instance = Config()
debug_value = Config.DEBUG
result = sub.helper()


def main():
    print(greet("world"))


if __name__ == "__main__":
    main()

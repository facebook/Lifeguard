# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.

# IMPORTANT: *Any* change to this file will kick off an upload of a new version of lifeguard-lazy-imports to PyPI.
#
# Cutting a release means changing the version below, regenerating Cargo.toml,
# and landing the change. The version has two forms:
# * stable:          "<major>.<minor>.<patch>"   (e.g. 1.2.0, 1.2.1)
# * dev pre-release: "<major>.<minor>.0-dev.<n>"  (e.g. 1.2.0-dev.4)
#
# To update the version manually, pick a transition that is allowed from the
# CURRENT form -- which bumps are legal depends on it:
# * From a dev pre-release "M.m.0-dev.N": increase the dev counter by 1 for the
#   next dev snapshot (1.2.0-dev.4 -> 1.2.0-dev.5), or drop -dev.<n> to
#   cut the stable release (1.2.0-dev.4 -> 1.2.0). Bumping the patch is NOT
#   allowed from a dev line.
# * From a stable "M.m.p": increase the patch number by 1 for a release with
#   only minor changes like bug fixes (1.2.0 -> 1.2.1).
# * From either form: start the next minor as a fresh "-dev.1" line or go
#   straight to stable (1.2.0 -> 1.3.0-dev.1 or 1.3.0) for major changes like new
#   features -- keeping a higher dev counter (e.g. 1.3.0-dev.4) is rejected. Bump
#   the major the same way (1.2.0 -> 2.0.0-dev.1 or 2.0.0) to indicate a
#   significant shift; this should almost never happen.
# * Do not include leading zeroes or anything else extra.
# Any other transition (skipping a counter, going backwards) is rejected by
# version validation. Cargo.toml is generated from the value below and must be
# regenerated whenever it changes.
#
# Meta-internal only: an automated dev release happens once a week, and releases
# are normally cut with the release tooling rather than by editing this file by
# hand -- validation runs as the //safer_lazy_imports/lifeguard:validate_version
# CI test and `arc autocargo -p lifeguard` regenerates Cargo.toml. See
# facebook/RELEASE.md for the full process; the usual entry point is:
#
#     buck2 run fbcode//safer_lazy_imports/lifeguard/facebook/scripts:release -- --bump-dev
VERSION = "0.1.0"

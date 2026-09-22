# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.

# IMPORTANT: *Any* change to this file will kick off an upload of a new version of lifeguard-lazy-imports to PyPI.
#
# Cutting a release means changing the version below, regenerating Cargo.toml,
# and landing the change. The version is "<major>.<minor>.<patch>" (e.g. 1.2.0,
# 1.2.1) -- there are no pre-release or dev versions.
#
# To update the version manually, pick one of these transitions:
# * Increase the patch by 1 for a release with only minor changes like bug
#   fixes (1.2.0 -> 1.2.1).
# * Increase the minor and reset the patch to 0 for changes like new features
#   (1.2.3 -> 1.3.0). This is what the weekly release cuts.
# * Increase the major and reset the rest to 0 to indicate a significant shift
#   (1.2.0 -> 2.0.0); this should almost never happen.
# * Do not include leading zeroes or anything else extra.
# Any other transition (skipping a number, going backwards) is rejected by
# version validation. Cargo.toml is generated from the value below and must be
# regenerated whenever it changes.
#
# Meta-internal only: an automated release happens once a week, and releases
# are normally cut with the release tooling rather than by editing this file by
# hand -- validation runs as the //safer_lazy_imports/lifeguard:validate_version
# CI test and `arc autocargo -p lifeguard` regenerates Cargo.toml. See
# facebook/RELEASE.md for the full process; the usual entry point is:
#
#     buck2 run fbcode//safer_lazy_imports/lifeguard/facebook/scripts:release -- --bump-minor
VERSION = "0.2.0"

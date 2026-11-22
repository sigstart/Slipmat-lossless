#!/bin/sh
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later

set -eu
scripts=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
sh "$scripts/fetch-fairplay-libs.sh" "$@"
exec sh "$scripts/fairplay/android-stubs.sh" "$@"

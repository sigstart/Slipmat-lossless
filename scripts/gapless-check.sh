#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Watch Slipmat's audio stream across a track boundary. A gapless handoff
# keeps the same stream alive. The current native player opens one per track.

set -uo pipefail

if ! command -v pactl >/dev/null; then
	echo "pactl not found — install libpulse (PipeWire's pulse shim)." >&2
	exit 1
fi

# Ignore notification sounds and other applications' streams.
slipmat_indices() {
	pactl list sink-inputs 2>/dev/null | awk '
		/^Sink Input #/ { idx = substr($3, 2); hit = 0 }
		tolower($0) ~ /slipmat/ { hit = 1 }
		/^[[:space:]]*$/ { if (hit && idx != "") print idx; idx = ""; hit = 0 }
		END { if (hit && idx != "") print idx }
	'
}

echo "Watching Slipmat's audio stream. Play across a track boundary."
echo "A remove and new event mark a reopened audio stream."
echo
printf 'Timestamps below are local time; the app logs UTC (offset %s).\n' \
	"$(date '+%:z')"
echo

known=$(slipmat_indices | tr '\n' ' ')
if [ -n "${known// /}" ]; then
	echo "Slipmat is already playing on sink-input(s): $known"
else
	echo "Slipmat is not playing yet — its stream will appear when you hit play."
fi
echo

pactl subscribe 2>/dev/null | while read -r line; do
	case "$line" in
	*"on sink-input"*) ;;
	*) continue ;;
	esac

	index=${line##*#}
	stamp=$(date '+%H:%M:%S.%3N')

	case "$line" in
	*remove*)
		# Already gone from `pactl list`, so this leans on what we saw when
		# it appeared.
		case " $known " in
		*" $index "*)
			echo "$stamp  Slipmat's stream #$index went away"
			echo "    ^^ the decoder stopped — that is an audible gap."
			known=${known// $index / }
			;;
		esac
		;;
	*)
		current=$(slipmat_indices | tr '\n' ' ')
		case " $current " in
		*" $index "*)
			[ -n "${known// /}" ] || echo "$stamp  Slipmat's stream #$index appeared"
			known=$current
			;;
		esac
		;;
	esac
done

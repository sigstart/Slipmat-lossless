# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later

"""Regenerate the synthetic clear ALAC test fixtures with Python and FFmpeg."""

from pathlib import Path
import subprocess


def encode(name, depth, rate, start, frames, left_step, right_step, movflags="+faststart"):
    minimum = -(1 << (depth - 1))
    maximum = -minimum - 1
    pcm = bytearray()
    for frame in range(start, start + frames):
        for sample in (minimum + frame * left_step, maximum - frame * right_step):
            pcm.extend(sample.to_bytes(depth // 8, "little", signed=True))

    subprocess.run(
        [
            "ffmpeg", "-nostdin", "-loglevel", "error", "-y",
            "-f", f"s{depth}le", "-ar", str(rate), "-ac", "2", "-i", "pipe:0",
            "-map_metadata", "-1", "-c:a", "alac",
            "-fflags", "+bitexact", "-flags:a", "+bitexact",
            "-movflags", movflags,
            str(Path(__file__).with_name(name)),
        ],
        input=pcm,
        check=True,
    )


if __name__ == "__main__":
    encode("stereo-16.m4a", 16, 44100, 0, 4099, 3, 7)
    encode("stereo-16-next.m4a", 16, 44100, 4099, 4101, 3, 7)
    encode("stereo-24.m4a", 24, 96000, 0, 4113, 19, 31)
    encode(
        "stereo-16-fragmented.m4a", 16, 44100, 0, 4099, 3, 7,
        "+empty_moov+default_base_moof+frag_every_frame+skip_trailer",
    )

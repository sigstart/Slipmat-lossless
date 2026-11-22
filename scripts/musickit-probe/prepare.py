#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Stage the MusicKit controller probe or Apple's playback sample."""

import argparse
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


PLATFORM_LIBRARIES = {
    "libandroid.so", "libOpenSLES.so", "libc.so", "libdl.so",
    "liblog.so", "libm.so", "libz.so",
}
SDK_ARCHIVES = ("MediaPlayback-release-1.1.4.aar", "MusicKitAuth-release-1.1.5.aar")


def native_libraries(directory):
    """Resolve the x86-64 library and its packaged dependencies."""
    names = ["libandroidappmusic.so", "libc++_shared.so"]
    libraries = {}
    while names:
        name = names.pop()
        if name in libraries or name in PLATFORM_LIBRARIES:
            continue
        if not re.fullmatch(r"lib[\w+.-]+\.so", name):
            raise ValueError(f"unexpected dependency name: {name}")
        source = directory / name
        if not source.is_file():
            raise ValueError(f"missing library: {source}")
        elf = subprocess.check_output(["readelf", "-h", "-d", source], text=True)
        if "Advanced Micro Devices X86-64" not in elf:
            raise ValueError(f"expected x86-64 ELF: {source}")
        libraries[name] = source
        names.extend(re.findall(r"\(NEEDED\).*?\[([^\]]+)\]", elf))
    return libraries


def stage_playback_sample(sample, output, developer_token):
    build = (sample / "build.gradle.kts").read_text(encoding="utf-8")
    build = build.replace("android {", "val probeAbi: String by project\n\nandroid {", 1)
    build = build.replace("defaultConfig {", 'defaultConfig {\n        ndk { abiFilters += probeAbi }', 1)
    (output / "build.gradle.kts").write_text(build, encoding="utf-8")
    shutil.copytree(sample / "src", output / "src", ignore=shutil.ignore_patterns("*.aar"))
    aar_directory = output / "src/main/libs"

    resources = output / "src/main/res/values/strings.xml"
    resource_xml = resources.read_text(encoding="utf-8")
    resource_xml, count = re.subn(
        r'(<string name="developer_token">)[^<]*(</string>)',
        lambda match: match[1] + developer_token + match[2],
        resource_xml,
    )
    if count != 1:
        raise ValueError("could not locate the sample's developer_token resource")
    resources.write_text(resource_xml, encoding="utf-8")

    manifest = output / "src/main/AndroidManifest.xml"
    manifest_xml = manifest.read_text(encoding="utf-8").replace(
        'android:allowBackup="true"', 'android:allowBackup="false"'
    )
    manifest_xml = manifest_xml.replace(
        '<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />',
        '<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />\n'
        '    <uses-permission android:name="android.permission.FOREGROUND_SERVICE_MEDIA_PLAYBACK" />',
    )
    manifest.write_text(manifest_xml, encoding="utf-8")
    return aar_directory


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("sdk", type=Path, help="extracted Android MusicKit SDK")
    parser.add_argument("--fairplay-libs", type=Path, help="x86-64 libraries; omit for the ARM64 control")
    parser.add_argument("--playback", action="store_true", help="stage Apple's complete test player")
    parser.add_argument("--developer-token-file", type=Path, help="local file containing a MusicKit developer token")
    args = parser.parse_args()
    if args.developer_token_file and not args.playback:
        parser.error("--developer-token-file requires --playback")
    try:
        for name in SDK_ARCHIVES:
            if not (args.sdk / name).is_file():
                raise ValueError(f"missing SDK archive: {args.sdk / name}")
        libraries = native_libraries(args.fairplay_libs) if args.fairplay_libs else {}
        developer_token = ""
        if args.developer_token_file:
            developer_token = args.developer_token_file.read_text(encoding="utf-8").strip()
            if not re.fullmatch(r"[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+){2}", developer_token):
                raise ValueError("expected a developer token in JWT format")
        sample = args.sdk / "sdk-test-app"
        if args.playback and not (sample / "build.gradle.kts").is_file():
            raise ValueError(f"missing Apple test player: {sample}")
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")

    templates = Path(__file__).resolve().parent
    output = Path(tempfile.mkdtemp(prefix="slipmat-musickit-probe-"))
    shutil.copyfile(templates / "settings.gradle.kts", output / "settings.gradle.kts")
    if args.playback:
        aar_directory = stage_playback_sample(sample, output, developer_token)
    else:
        shutil.copyfile(templates / "build.gradle.kts", output / "build.gradle.kts")
        shutil.copytree(templates / "src", output / "src")
        aar_directory = output / "libs"
    aar_directory.mkdir(exist_ok=True)
    for name in SDK_ARCHIVES:
        shutil.copyfile(args.sdk / name, aar_directory / name)

    abi = "x86_64" if args.fairplay_libs else "arm64-v8a"
    (output / "gradle.properties").write_text(
        f"probeAbi={abi}\nandroid.useAndroidX=true\n", encoding="utf-8"
    )
    if libraries:
        jni = output / "src/main/jniLibs" / abi
        jni.mkdir(parents=True)
        for name, source in sorted(libraries.items()):
            # Keep the SDK's Java loadLibrary call and native interfaces intact.
            target = "libappleMusicSDK.so" if name == "libandroidappmusic.so" else name
            shutil.copyfile(source, jni / target)
    if args.playback and not developer_token:
        print("Sample staged without a developer token; streaming requires one.", file=sys.stderr)
    print(output)


if __name__ == "__main__":
    main()

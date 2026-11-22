# MusicKit controller startup probe

## Run Apple's ARM64 SDK on an x86-64 Linux desktop

Use an x86-64 Waydroid image with an ARM64 native bridge. Waydroid runs
Android in a container. The bridge translates Apple's ARM64 native library
while Android provides the Java framework and audio services. Check
`ro.product.cpu.abilist` with `getprop` inside Waydroid's Android shell. It
should include `arm64-v8a` before you install the ARM64 sample.

Use the sample build below with `--fairplay-libs` omitted. This packages the
original ARM64 libraries from Apple's AAR. With the Waydroid session running,
install and launch the result from Linux:

```sh
waydroid app install "$app_dir/build/outputs/apk/debug/SlipmatMusicKitProbe-debug.apk"
waydroid app launch com.apple.android.music.sdk.testapp
```

Sign in, select **Lossless**, and play a full track. Confirm the badge says
**Lossless**, the position advances, and you hear audio on the host. This
checks playback before adding any Slipmat integration. Stop the Android
session with `waydroid session stop` after the trial.

## Stream a track in Apple's sample app

Use this test to check sign-in, playback, the reported audio format, and sound
in one run. Use a disposable x86-64 Android environment. The supplied SDK
sample's developer token expired on 6 June 2026. Provide your own valid MusicKit
developer token in a local text file. Keep the token file and the APK private.
The app handles Apple Account sign-in through MusicKitAuth.

From the `Slipmat/` repository root, with JDK 17, Gradle 8.4, Android SDK
Platform 34, Build Tools 34.0.0, and `adb` available:

```sh
sdk="/path/to/extracted/Android-MusicKit-SDK-1.1.4"
app_dir=$(python3 scripts/musickit-probe/prepare.py "$sdk" --playback \
  --fairplay-libs "$PWD/vendor/fairplay/x86_64/rootfs/system/lib64" \
  --developer-token-file /path/to/your/developer-token.txt)
gradle -p "$app_dir" assembleDebug
adb install -r "$app_dir/build/outputs/apk/debug/SlipmatMusicKitProbe-debug.apk"
adb shell am start -S -n com.apple.android.music.sdk.testapp/.activity.MainActivity
```

In Settings, sign in and choose **Lossless**. Play a full catalog track and
check that the playback position advances, the Now Playing badge reports
**Lossless**, and you hear audio. If it fails, note whether the app opened,
sign-in completed, or playback failed. A native crash or linker error can be
collected with `adb logcat -b crash -d`. This test uses the SDK's reported audio
format and audible output. It does not measure the host's output bit depth
or sample rate.

## Optional startup diagnostics

The probe loads the candidate x86-64 binary under the same name Apple's sample
uses and calls the SDK's public controller factory. It keeps the Java classes
from the official AAR and uses real Android platform libraries from the guest.
It has no internet permission and uses a signed-out token provider. Use a
disposable Android test environment.

## Build and run

Use JDK 17, Gradle 8.4, Android SDK Platform 34, and Android platform tools
(`adb`). Set `ANDROID_HOME` to the SDK directory. Boot an Android x86-64
emulator or test device, then run from the `Slipmat/` repository root.

```sh
adb devices
adb shell getprop ro.product.cpu.abilist

sdk="/path/to/extracted/Android-MusicKit-SDK-1.1.4"
probe_dir=$(python3 scripts/musickit-probe/prepare.py "$sdk" \
  --fairplay-libs "$PWD/vendor/fairplay/x86_64/rootfs/system/lib64")

gradle -p "$probe_dir" assembleDebug
apk="$probe_dir/build/outputs/apk/debug/SlipmatMusicKitProbe-debug.apk"
find "$probe_dir/build/outputs/apk/debug" -maxdepth 1 -name '*.apk'
adb install -r "$apk"
adb shell am start -S -n dev.slipmat.musickitprobe/.ProbeActivity
adb logcat -d -t 1500 -s SlipmatMusicKitProbe AndroidRuntime linker libc DEBUG
```

The staging script prints a fresh project path. If Gradle names the APK
differently, use the path printed by `find`. If multiple devices are connected,
select the test device with `adb -s SERIAL` for the install, launch, and log.

## Interpret the result

Share the first `FAIL` and its stack trace. If the process crashes, share the
last `BEGIN` marker and the corresponding crash lines. A native load error
identifies a missing dependency, incompatible export, or initialization failure.
A signed-out controller failure may come from the empty token provider.

For a control run, omit `--fairplay-libs` and install the build in an ARM64 Android
environment. The same probe then uses Apple's native ARM64 library. An error
that reproduces in both runs may be a harness or SDK setup issue.

A startup pass verifies controller creation and state queries. A separate
authenticated run is needed to check queue preparation, an ALAC report from
`getAudioVariant()`, and audible audio output.

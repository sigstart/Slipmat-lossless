// SPDX-License-Identifier: GPL-3.0-or-later
package dev.slipmat.musickitprobe;

import android.app.Activity;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import android.widget.TextView;

import com.apple.android.music.playback.controller.MediaPlayerController;
import com.apple.android.music.playback.controller.MediaPlayerControllerFactory;
import com.apple.android.sdk.authentication.TokenProvider;

import java.util.Arrays;

public final class ProbeActivity extends Activity {
    private static final String TAG = "SlipmatMusicKitProbe";

    @Override
    public void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        TextView status = new TextView(this);
        status.setText("Running MusicKit startup probe. See logcat for stages.");
        setContentView(status);
        status.post(() -> status.setText(runProbe()));
    }

    private String runProbe() {
        String stage = "load C++ runtime";
        Log.i(TAG, "Start: Android API " + Build.VERSION.SDK_INT
                + ", ABIs " + Arrays.toString(Build.SUPPORTED_ABIS));
        try {
            System.setProperty("org.bytedeco.javacpp.maxphysicalbytes", "0");
            System.setProperty("org.bytedeco.javacpp.maxbytes", "0");
            Log.i(TAG, "BEGIN: " + stage);
            System.loadLibrary("c++_shared");
            Log.i(TAG, "PASS: " + stage);
            stage = "load MusicKit native library and run JNI_OnLoad";
            Log.i(TAG, "BEGIN: " + stage);
            System.loadLibrary("appleMusicSDK");
            Log.i(TAG, "PASS: " + stage);

            stage = "create controller without an Apple account";
            Log.i(TAG, "BEGIN: " + stage);
            TokenProvider signedOut = new TokenProvider() {
                @Override public String getDeveloperToken() { return null; }
                @Override public String getUserToken() { return null; }
                @Override public String getAppleMusicStoreFrontId() { return null; }
            };
            MediaPlayerController controller = MediaPlayerControllerFactory.createLocalController(
                    getApplicationContext(), new Handler(Looper.getMainLooper()), signedOut);
            if (controller == null) {
                throw new IllegalStateException("MusicKit returned a null controller");
            }
            Log.i(TAG, "PASS: " + stage);

            stage = "read playback state and audio variant";
            Log.i(TAG, "BEGIN: " + stage);
            int state = controller.getPlaybackState();
            int variant = controller.getAudioVariant();
            Log.i(TAG, "PASS: " + stage + " (state=" + state
                    + ", variant=" + variant + ")");
            stage = "release controller";
            Log.i(TAG, "BEGIN: " + stage);
            controller.release();
            Log.i(TAG, "PASS: " + stage);
            return "Startup probe passed. Playback and audio output are untested.";
        } catch (RuntimeException | LinkageError e) {
            Log.e(TAG, "FAIL: " + stage, e);
            return "Failed at " + stage + ". See logcat for details.";
        }
    }
}

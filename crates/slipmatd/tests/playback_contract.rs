// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use slipmat_core::ipc::{Event, Request, Stage, Transport};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::{
    UnixStream,
    unix::{OwnedReadHalf, OwnedWriteHalf},
};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

const WAIT: Duration = Duration::from_secs(10);
const MPRIS_NAME: &str = "org.mpris.MediaPlayer2.Slipmat";
const MPRIS_PLAYER: &str = "org.mpris.MediaPlayer2.Player";

#[tokio::test]
async fn normal_play_uses_the_configured_queue() {
    let home = TestHome::new();
    let launcher = home.0.join("provider");
    fs::write(&launcher, include_bytes!("fixtures/queue-provider")).unwrap();
    fs::set_permissions(&launcher, Permissions::from_mode(0o700)).unwrap();
    let state = home.0.join("state/slipmat");
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("fairplay-provider"),
        format!("{}\n", launcher.display()),
    )
    .unwrap();
    let runtime = home.0.join("run");
    fs::set_permissions(&runtime, Permissions::from_mode(0o700)).unwrap();

    let (bus, bus_address) = home.private_bus().await;
    let mut daemon = home
        .command(env!("CARGO_BIN_EXE_slipmatd"))
        .env("DBUS_SESSION_BUS_ADDRESS", &bus_address)
        .env("DBUS_SYSTEM_BUS_ADDRESS", home.address("no-system-bus"))
        .env(
            "PULSE_SERVER",
            format!("unix:{}", home.0.join("no-pulse").display()),
        )
        .spawn()
        .unwrap();
    let socket = runtime.join("slipmat.sock");
    let mut feed = Wire::new(
        timeout(WAIT, async {
            loop {
                if let Ok(stream) = UnixStream::connect(&socket).await {
                    break stream;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap(),
    );
    let mut commands = Wire::new(UnixStream::connect(&socket).await.unwrap());
    feed.request(Request::Subscribe).await;
    assert!(matches!(feed.event().await, Event::Snapshot(_)));
    assert!(matches!(
        feed.event().await,
        Event::PlaybackCapabilities {
            seek: false,
            shuffle: false,
            repeat: false
        }
    ));

    assert!(matches!(
        feed.event().await,
        Event::SignInRequired { required: false }
    ));
    commands.request(Request::Stage).await;
    assert_eq!(commands.stage_event().await, Stage::SignedOut);

    commands
        .request(Request::Play {
            ids: ["111", "111", "333"].map(str::to_owned).to_vec(),
            index: 1,
            start: Default::default(),
        })
        .await;
    feed.expect_queue(1, &["111", "111", "333"]).await;
    feed.until(|event| matches!(event, Event::SignInRequired { required: true }))
        .await;

    let mut auth = Wire::new(
        UnixStream::connect(runtime.join("slipmat-auth.sock"))
            .await
            .unwrap(),
    );
    auth.send(json!({"type":"begin"})).await;
    auth.expect_command(json!({"type":"credentialsRequired"}))
        .await;
    auth.write_line(
        r#"{"type":"credentials","account":"test@example.invalid","password":"fixture-password"}"#,
    )
    .await;
    auth.expect_command(json!({"type":"signedIn"})).await;
    feed.expect_position("111", 1250, true).await;

    let mpris = bus.is_some() && home.wait_for_mpris(&bus_address).await;
    if mpris {
        home.expect_playback_status(&bus_address, "Playing").await;
        home.mpris_call(&bus_address, "org.mpris.MediaPlayer2.Player.Pause", &[])
            .await;
    } else {
        commands.request(Request::Transport(Transport::Pause)).await;
    }
    feed.expect_position("111", 1250, false).await;
    commands.request(Request::Transport(Transport::Play)).await;
    feed.expect_position("111", 1250, true).await;
    commands
        .request(Request::Enqueue {
            ids: vec!["222".into()],
            next: true,
        })
        .await;
    feed.expect_queue(1, &["111", "111", "222", "333"]).await;
    commands
        .request(Request::MoveInQueue { from: 0, to: 3 })
        .await;
    feed.expect_queue(0, &["111", "222", "333", "111"]).await;

    commands.request(Request::Transport(Transport::Next)).await;
    feed.expect_queue(1, &["111", "222", "333", "111"]).await;
    feed.expect_queue(2, &["111", "222", "333", "111"]).await;
    feed.expect_position("333", 2300, true).await;
    commands.request(Request::JumpTo { index: 3 }).await;
    feed.expect_queue(3, &["111", "222", "333", "111"]).await;
    feed.expect_position("111", 1250, true).await;
    commands.request(Request::ClearQueue).await;
    feed.expect_queue(0, &[]).await;
    feed.until(
        |event| matches!(event, Event::Snapshot(snap) if !snap.playing && snap.track_id.is_none()),
    )
    .await;

    commands
        .request(Request::Play {
            ids: ["444", "444"].map(str::to_owned).to_vec(),
            index: 0,
            start: Default::default(),
        })
        .await;
    feed.expect_queue(0, &["444", "444"]).await;
    feed.expect_position("444", 3600, true).await;
    let duplicate_tracks = if mpris {
        let tracks = home.mpris_tracks(&bus_address).await;
        assert_eq!(tracks.len(), 2);
        assert_ne!(tracks[0], tracks[1]);
        assert_eq!(home.mpris_track_id(&bus_address).await, tracks[0]);
        Some(tracks)
    } else {
        None
    };
    commands.request(Request::Transport(Transport::Pause)).await;
    feed.expect_position("444", 3600, false).await;
    commands.request(Request::Transport(Transport::Play)).await;
    // The provider completes the first occurrence after resume.
    feed.expect_queue(1, &["444", "444"]).await;
    feed.expect_position("444", 400, true).await;
    if let Some(tracks) = duplicate_tracks {
        assert_eq!(home.mpris_tracks(&bus_address).await, tracks);
        assert_eq!(home.mpris_track_id(&bus_address).await, tracks[1]);
    }

    commands
        .request(Request::Play {
            ids: vec!["111".into()],
            index: 0,
            start: Default::default(),
        })
        .await;
    feed.expect_position("111", 1250, true).await;
    assert!(state.join("session.json").is_file());
    auth.send(json!({"type":"signOut"})).await;
    auth.expect_command(json!({"type":"signedOut"})).await;
    commands.request(Request::Queue).await;
    commands.expect_queue(0, &[]).await;
    commands.request(Request::Snapshot).await;
    assert!(matches!(
        commands.event().await,
        Event::Snapshot(snapshot) if snapshot.track_id.is_none() && !snapshot.playing
    ));
    assert!(!state.join("session.json").exists());

    let mut idle_main = Wire::new(UnixStream::connect(&socket).await.unwrap());
    let mut idle_auth = Wire::new(
        UnixStream::connect(runtime.join("slipmat-auth.sock"))
            .await
            .unwrap(),
    );
    timeout(Duration::from_secs(8), async {
        let (main, auth) = tokio::join!(idle_main.read.next_line(), idle_auth.read.next_line());
        assert!(main.unwrap().is_none());
        assert!(auth.unwrap().is_none());
    })
    .await
    .unwrap();
    commands.request(Request::Stage).await;
    assert_eq!(commands.stage_event().await, Stage::SignedOut);

    commands.write.shutdown().await.unwrap();
    assert!(
        timeout(WAIT, commands.read.next_line())
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    feed.request(Request::Quit).await;
    assert!(
        timeout(WAIT, daemon.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn stale_provider_configuration_keeps_the_daemon_available() {
    let home = TestHome::new();
    let runtime = home.0.join("run");
    fs::set_permissions(&runtime, Permissions::from_mode(0o700)).unwrap();
    let state = home.0.join("state/slipmat");
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("fairplay-provider"),
        format!("{}\n", home.0.join("missing-launcher").display()),
    )
    .unwrap();

    let mut daemon = home
        .command(env!("CARGO_BIN_EXE_slipmatd"))
        .env("DBUS_SESSION_BUS_ADDRESS", home.address("no-bus"))
        .spawn()
        .unwrap();
    let mut commands = Wire::connect(&runtime.join("slipmat.sock")).await;
    commands.request(Request::Stage).await;
    assert!(
        matches!(commands.stage_event().await, Stage::Broken { detail } if detail.contains("launcher path"))
    );
    assert!(!runtime.join("slipmat-auth.sock").exists());
    commands.request(Request::Quit).await;
    assert!(
        timeout(WAIT, daemon.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

const PROFILE_PROVIDER: &str = r#"#!/bin/sh
set -eu
IFS= read -r begin
[ "$begin" = '{"type":"begin"}' ]
files="$1/files"
if [ "$2" = sign-in ]; then
    /bin/mkdir -p "$files/mpl_db"
    printf 'ready' > "$files/session-ready"
    printf 'fixture-accounts' > "$files/mpl_db/accounts.sqlitedb"
else
    [ "$2" = restore ]
    [ "$(/bin/cat "$files/session-ready")" = ready ]
    [ "$(/bin/cat "$files/mpl_db/accounts.sqlitedb")" = fixture-accounts ]
fi
printf '%s\n' '{"type":"signedIn","catalogue":{"developerToken":"fixture-dev","musicUserToken":"fixture-user","storefront":"us","authorized":true}}'
while IFS= read -r request; do :; done
"#;

// Stands in for Secret Service. The slow store is what a quit must wait out.
const FAKE_SECRET_TOOL: &str = r#"#!/bin/sh
set -eu
keyring="$FAKE_KEYRING"
case "$1" in
    lookup)
        [ "$2 $3 $4" = 'application dev.miguelrincon.Slipmat profile' ]
        [ -f "$keyring" ] && [ "$(/bin/cat "$keyring.profile")" = "$5" ] || exit 1
        /bin/cat "$keyring"
        ;;
    store)
        [ "$2" = '--label=Slipmat FairPlay session key' ]
        [ "$3 $4 $5" = 'application dev.miguelrincon.Slipmat profile' ]
        /bin/sleep 0.4
        /bin/cat > "$keyring"
        printf '%s' "$6" > "$keyring.profile"
        ;;
    *) exit 1 ;;
esac
"#;

#[tokio::test]
async fn a_saved_session_survives_quit_and_restores_until_sign_out() {
    let home = TestHome::new();
    assert!(
        home.on_tmpfs(),
        "{} must be tmpfs for the provider profile",
        home.0.display()
    );
    let runtime = home.0.join("run");
    fs::set_permissions(&runtime, Permissions::from_mode(0o700)).unwrap();
    let launcher = home.0.join("provider");
    fs::write(&launcher, PROFILE_PROVIDER).unwrap();
    fs::set_permissions(&launcher, Permissions::from_mode(0o700)).unwrap();
    let bin = home.0.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("secret-tool"), FAKE_SECRET_TOOL).unwrap();
    fs::set_permissions(bin.join("secret-tool"), Permissions::from_mode(0o700)).unwrap();
    let state = home.0.join("state/slipmat");
    let archive = state.join("fairplay-session.enc");
    let keyring = home.0.join("keyring");

    let spawn = || {
        home.command(env!("CARGO_BIN_EXE_slipmatd"))
            .arg("--fairplay-provider")
            .arg(&launcher)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("FAKE_KEYRING", &keyring)
            .env("DBUS_SESSION_BUS_ADDRESS", home.address("no-bus"))
            .env(
                "PULSE_SERVER",
                format!("unix:{}", home.0.join("no-pulse").display()),
            )
            .spawn()
            .unwrap()
    };
    let mut daemon = spawn();
    let mut commands = Wire::connect(&runtime.join("slipmat.sock")).await;
    let mut auth = Wire::connect(&runtime.join("slipmat-auth.sock")).await;
    auth.send(json!({"type":"begin"})).await;
    auth.expect_command(json!({"type":"signedIn"})).await;
    commands.request(Request::Quit).await;
    assert!(
        timeout(WAIT, daemon.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let sealed = fs::read(&archive).unwrap();
    assert!(
        !sealed
            .windows(b"fixture-accounts".len())
            .any(|window| window == b"fixture-accounts"),
        "the archive holds the profile in the clear"
    );
    assert_eq!(
        fs::read_to_string(keyring.with_extension("profile")).unwrap(),
        archive.display().to_string()
    );
    assert!(
        !runtime.join("slipmat-fairplay-profile").exists(),
        "the decrypted profile outlived the daemon"
    );

    let mut daemon = spawn();
    let mut auth = Wire::connect(&runtime.join("slipmat-auth.sock")).await;
    timeout(WAIT, async {
        loop {
            auth.send(json!({"type":"status"})).await;
            let reply: Value = serde_json::from_str(&auth.read_line().await).unwrap();
            if reply == json!({"type":"signedIn"}) {
                break;
            }
            assert_eq!(reply, json!({"type":"busy"}));
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    auth.send(json!({"type":"signOut"})).await;
    auth.expect_command(json!({"type":"signedOut"})).await;
    assert!(!archive.exists());
    let mut commands = Wire::connect(&runtime.join("slipmat.sock")).await;
    commands.request(Request::Quit).await;
    assert!(
        timeout(WAIT, daemon.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

struct Wire {
    read: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl Wire {
    async fn connect(path: &Path) -> Self {
        timeout(WAIT, async {
            loop {
                if let Ok(stream) = UnixStream::connect(path).await {
                    return Self::new(stream);
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }

    fn new(stream: UnixStream) -> Self {
        let (read, write) = stream.into_split();
        Self {
            read: BufReader::new(read).lines(),
            write,
        }
    }

    async fn write_line(&mut self, line: &str) {
        self.write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn send(&mut self, frame: Value) {
        self.write_line(&frame.to_string()).await;
    }

    async fn request(&mut self, request: Request) {
        self.send(serde_json::to_value(request).unwrap()).await;
    }

    async fn read_line(&mut self) -> String {
        self.read
            .next_line()
            .await
            .unwrap()
            .expect("peer disconnected")
    }

    async fn event(&mut self) -> Event {
        timeout(WAIT, async {
            serde_json::from_str(&self.read_line().await).unwrap()
        })
        .await
        .expect("timed out reading slipmatd IPC")
    }

    async fn until(&mut self, matches: impl Fn(Event) -> bool) {
        timeout(WAIT, async {
            loop {
                let event = self.event().await;
                assert!(
                    !matches!(event, Event::Error { .. }),
                    "unexpected event: {event:?}"
                );
                if matches(event) {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for the expected event");
    }

    async fn expect_position(&mut self, id: &str, ms: u64, playing: bool) {
        self.until(|event| {
            matches!(event, Event::Snapshot(snap)
            if snap.track_id.as_deref() == Some(id) && snap.position_ms == ms
                && snap.playing == playing && !snap.busy)
        })
        .await;
    }

    async fn stage_event(&mut self) -> Stage {
        timeout(WAIT, async {
            loop {
                if let Event::Stage(stage) = self.event().await {
                    return stage;
                }
            }
        })
        .await
        .expect("timed out waiting for a stage notification")
    }

    async fn expect_queue(&mut self, index: usize, songs: &[&str]) {
        timeout(WAIT, async {
            loop {
                if let Event::Queue { items, position } = self.event().await {
                    assert_eq!(position, index);
                    let ids: Vec<_> = items
                        .iter()
                        .map(|item| item.id.as_deref().unwrap())
                        .collect();
                    assert_eq!(ids, songs);
                    return;
                }
            }
        })
        .await
        .expect("timed out waiting for a queue notification");
    }

    async fn expect_command(&mut self, expected: Value) {
        let line = timeout(WAIT, self.read_line())
            .await
            .unwrap_or_else(|_| panic!("missing child command {expected}"));
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), expected);
    }
}

struct TestHome(PathBuf);

impl TestHome {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Keep the private sockets within the Unix path length limit. The
        // provider profile needs tmpfs, which /dev/shm is wherever it exists.
        let base = ["/dev/shm", "/tmp"]
            .into_iter()
            .map(Path::new)
            .find(|base| base.is_dir())
            .unwrap();
        let root = Self(base.join(format!("slipmat-{}-{unique}", std::process::id())));
        DirBuilder::new().mode(0o700).create(&root.0).unwrap();
        for dir in ["run", "home", "cache", "config", "data", "state"] {
            fs::create_dir(root.0.join(dir)).unwrap();
        }
        root
    }

    fn on_tmpfs(&self) -> bool {
        std::process::Command::new("stat")
            .args(["-f", "-c", "%T"])
            .arg(&self.0)
            .output()
            .is_ok_and(|output| output.stdout == b"tmpfs\n")
    }

    fn command(&self, executable: &str) -> Command {
        let mut command = Command::new(executable);
        command
            .kill_on_drop(true)
            .env_clear()
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .env("HOME", self.0.join("home"))
            .env("XDG_RUNTIME_DIR", self.0.join("run"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_CONFIG_DIRS", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_DATA_DIRS", self.0.join("data"))
            .env("XDG_STATE_HOME", self.0.join("state"));
        command
    }

    fn address(&self, socket: &str) -> String {
        format!("unix:path={}", self.0.join(socket).display())
    }

    async fn private_bus(&self) -> (Option<Child>, String) {
        let address = self.address("bus");
        let result = self
            .command("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!("--address={address}"))
            .stdout(Stdio::piped())
            .spawn();
        let mut bus = match result {
            Ok(bus) => bus,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                eprintln!("dbus-daemon unavailable, testing with a private disconnected address");
                return (None, address);
            }
            Err(err) => panic!("could not start private D-Bus: {err}"),
        };
        let mut lines = BufReader::new(bus.stdout.take().unwrap()).lines();
        let bound_address = timeout(WAIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("private D-Bus exited without an address");
        (Some(bus), bound_address)
    }

    async fn wait_for_mpris(&self, address: &str) -> bool {
        let result = timeout(
            WAIT,
            self.command("gdbus")
                .args(["wait", "--address", address, MPRIS_NAME])
                .output(),
        )
        .await
        .expect("timed out waiting for slipmatd to export MPRIS");
        let output = match result {
            Ok(output) => output,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                eprintln!("gdbus unavailable, checking Pause over Slipmat IPC");
                return false;
            }
            Err(err) => panic!("could not run gdbus wait: {err}"),
        };
        assert!(
            output.status.success(),
            "gdbus wait failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    async fn mpris_call(&self, address: &str, method: &str, args: &[&str]) -> String {
        let output = timeout(
            WAIT,
            self.command("gdbus")
                .args([
                    "call",
                    "--address",
                    address,
                    "--dest",
                    MPRIS_NAME,
                    "--object-path",
                    "/org/mpris/MediaPlayer2",
                    "--method",
                    method,
                ])
                .args(args)
                .output(),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out calling {method}"))
        .expect("run gdbus call");
        assert!(
            output.status.success(),
            "{method} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    async fn expect_playback_status(&self, address: &str, status: &str) {
        let expected = format!("(<'{status}'>,)");
        let mut observed = String::new();
        let matched = timeout(WAIT, async {
            loop {
                observed = self
                    .mpris_call(
                        address,
                        "org.freedesktop.DBus.Properties.Get",
                        &[MPRIS_PLAYER, "PlaybackStatus"],
                    )
                    .await;
                if observed.trim() == expected {
                    return;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        assert!(
            matched.is_ok(),
            "timed out waiting for MPRIS PlaybackStatus {status}, last response {observed:?}"
        );
    }

    async fn mpris_tracks(&self, address: &str) -> Vec<String> {
        self.mpris_call(
            address,
            "org.freedesktop.DBus.Properties.Get",
            &["org.mpris.MediaPlayer2.TrackList", "Tracks"],
        )
        .await
        .split('\'')
        .filter(|part| part.starts_with('/'))
        .map(str::to_owned)
        .collect()
    }

    async fn mpris_track_id(&self, address: &str) -> String {
        let metadata = self
            .mpris_call(
                address,
                "org.freedesktop.DBus.Properties.Get",
                &[MPRIS_PLAYER, "Metadata"],
            )
            .await;
        let (_, track_id) = metadata.split_once("'mpris:trackid':").unwrap();
        track_id.split('\'').nth(1).unwrap().to_owned()
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

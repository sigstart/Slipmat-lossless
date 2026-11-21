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
async fn normal_play_uses_the_configured_native_queue() {
    let home = TestHome::new();
    let launcher = home.0.join("provider");
    fs::write(&launcher, include_bytes!("fixtures/native-queue-provider")).unwrap();
    fs::set_permissions(&launcher, Permissions::from_mode(0o700)).unwrap();
    let state = home.0.join("state/slipmat");
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("native-provider"),
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
            native: true,
            seek: false,
            shuffle: false,
            repeat: false
        }
    ));

    assert!(matches!(
        feed.event().await,
        Event::NativeSignIn { required: false }
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
    feed.until(|event| matches!(event, Event::NativeSignIn { required: true }))
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
    feed.expect_native_position("111", 1250, true).await;

    let mpris = bus.is_some() && home.wait_for_mpris(&bus_address).await;
    if mpris {
        home.expect_playback_status(&bus_address, "Playing").await;
        home.mpris_call(&bus_address, "org.mpris.MediaPlayer2.Player.Pause", &[])
            .await;
    } else {
        commands.request(Request::Transport(Transport::Pause)).await;
    }
    feed.expect_native_position("111", 1250, false).await;
    commands.request(Request::Transport(Transport::Play)).await;
    feed.expect_native_position("111", 1250, true).await;
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
    feed.expect_native_position("333", 2300, true).await;
    commands.request(Request::JumpTo { index: 3 }).await;
    feed.expect_queue(3, &["111", "222", "333", "111"]).await;
    feed.expect_native_position("111", 1250, true).await;
    commands.request(Request::ClearQueue).await;
    feed.expect_queue(0, &[]).await;
    feed.until(
        |event| matches!(event, Event::Snapshot(snap) if !snap.playing && snap.track_id.is_none()),
    )
    .await;

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

struct Wire {
    read: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl Wire {
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

    async fn expect_native_position(&mut self, id: &str, ms: u64, playing: bool) {
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
        // Keep the private sockets within the Unix path length limit.
        let root = Self(Path::new("/tmp").join(format!("slipmat-{}-{unique}", std::process::id())));
        DirBuilder::new().mode(0o700).create(&root.0).unwrap();
        for dir in ["run", "home", "cache", "config", "data", "state"] {
            fs::create_dir(root.0.join(dir)).unwrap();
        }
        root
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
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

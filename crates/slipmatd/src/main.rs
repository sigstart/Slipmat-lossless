// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared daemon for native playback, the queue, and library operations.
//!
//! The terminal client and GTK window subscribe to the same player state. One
//! daemon owns the FairPlay subscriber session and the Chromium library profile.
//!
//! Single-threaded on purpose. `mpris_server::Player` is `!Send`, the sidecar's
//! stdout reader is a local stream, and there is nothing here worth a thread
//! pool — this process waits on I/O, it does not compute.

mod bus;
mod heal;
mod library;
mod mixer;
mod native_auth;
mod native_queue;
mod serve;
mod state;
mod watchdog;

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let mode = mode()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("slipmatd=info,slipmat_core=info")),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the runtime")?;

    // A `LocalSet`, because the things this process holds cannot be moved
    // between threads and there is no reason to want them to be.
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        match mode {
            Mode::Full { provider, native } => serve::run(provider, native).await,
            Mode::NativeTrial(provider) => native_auth::run(provider).await,
        }
    })
}

enum Mode {
    Full {
        provider: Option<PathBuf>,
        native: bool,
    },
    NativeTrial(PathBuf),
}

fn mode() -> Result<Mode> {
    let mut args = std::env::args_os().skip(1);
    let Some(flag) = args.next() else {
        return Ok(Mode::Full {
            provider: configured_provider(),
            native: true,
        });
    };
    if flag == "--web-playback" && args.next().is_none() {
        return Ok(Mode::Full {
            provider: None,
            native: false,
        });
    }
    let usage = "usage: slipmatd [--web-playback | --native-sign-in-provider /absolute/path | --native-trial-provider /absolute/path]";
    let path = PathBuf::from(args.next().context(usage)?);
    ensure!(args.next().is_none() && path.is_absolute(), "{usage}");
    if flag == "--native-trial-provider" {
        Ok(Mode::NativeTrial(path))
    } else {
        ensure!(flag == "--native-sign-in-provider", "{usage}");
        Ok(Mode::Full {
            provider: Some(path),
            native: true,
        })
    }
}

fn configured_provider() -> Option<PathBuf> {
    let path = slipmat_core::paths::state_dir()?.join("native-provider");
    let contents = std::fs::read_to_string(path).ok()?;
    let launcher = PathBuf::from(contents.trim_end());
    launcher.is_absolute().then_some(launcher)
}

// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared daemon for authentication, Apple Music, the queue, and MPRIS.

mod auth;
mod bus;
mod library;
mod mixer;
mod profile_store;
mod provider;
mod queue;
mod serve;
mod state;

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let provider = provider()?;
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
    local.block_on(&runtime, serve::run(provider))
}

/// The provider launcher from `--fairplay-provider`, else the saved setting.
fn provider() -> Result<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    let Some(flag) = args.next() else {
        return Ok(configured_provider());
    };
    let usage = "usage: slipmatd [--fairplay-provider /absolute/path]";
    ensure!(flag == "--fairplay-provider", "{usage}");
    let path = PathBuf::from(args.next().context(usage)?);
    ensure!(args.next().is_none() && path.is_absolute(), "{usage}");
    Ok(Some(path))
}

fn configured_provider() -> Option<PathBuf> {
    let path = slipmat_core::paths::state_dir()?.join("fairplay-provider");
    let contents = std::fs::read_to_string(path).ok()?;
    let launcher = PathBuf::from(contents.trim_end());
    launcher.is_absolute().then_some(launcher)
}

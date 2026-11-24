// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! `api.music.apple.com` — everything the app *shows*, as opposed to plays.
//!
//! The split is the whole architecture in one line: metadata comes down this
//! path as JSON and is rendered with native widgets; only the audio goes
//! through the FairPlay provider.

// `Client::delete` waits for a removal route (CLAUDE.md, writing to the library).
#![allow(dead_code)]

pub mod asset_shape;
pub mod client;
pub mod types;

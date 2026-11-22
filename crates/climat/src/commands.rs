// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! One-off commands that answer and exit, without the player UI.
//!
//! `current` and `queue` print what the daemon is holding, with the catalogue
//! ids nothing else shows: the desktop's media controls name tracks by queue
//! position. `check` asks the daemon to decode a track to nowhere, as fast as
//! it downloads, and prints how that ended — the way to find out whether a
//! track plays through without listening to all of it.

use std::ffi::OsString;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use slipmat_core::ipc::{Event, QueueItem, Request, Snapshot};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::link::{self, Incoming, Link};

/// How long the daemon has to say what it holds.
const STATE_TIMEOUT: Duration = Duration::from_secs(5);

pub enum Command {
    Current,
    Queue,
    /// A catalogue id, or the current track's.
    Check(Option<String>),
}

impl Command {
    pub fn parse(args: &[OsString]) -> Result<Option<Self>> {
        let mut args = args.iter().map(|arg| arg.to_str());
        let command = match args.next().flatten() {
            Some("current") => Self::Current,
            Some("queue") => Self::Queue,
            Some("check") => Self::Check(args.next().flatten().map(str::to_owned)),
            _ => return Ok(None),
        };
        if args.next().is_some() {
            bail!("Too many arguments.");
        }
        Ok(Some(command))
    }
}

pub async fn run(command: Command) -> Result<()> {
    let (link, mut events) = link::connect().await?;
    let (snapshot, queue) = tokio::time::timeout(STATE_TIMEOUT, state(&mut events))
        .await
        .context("Timed out waiting for slipmatd.")??;
    match command {
        Command::Current => {
            let id = snapshot
                .track_id
                .as_deref()
                .context("Nothing is playing.")?;
            println!("{id}\t{}", describe(&snapshot.title, &snapshot.artist));
            Ok(())
        }
        Command::Queue => {
            let (items, position) = queue;
            if items.is_empty() {
                bail!("The queue is empty.");
            }
            for (index, item) in items.iter().enumerate() {
                let marker = if index == position { '>' } else { ' ' };
                let id = item.id.as_deref().unwrap_or("-");
                println!("{marker} {id}\t{}", describe(&item.title, &item.artist));
            }
            Ok(())
        }
        Command::Check(id) => {
            let id = id
                .or(snapshot.track_id.clone())
                .context("Nothing is playing; name a track: climat check <id>")?;
            let name = name_of(&id, &snapshot, &queue.0);
            check(&link, &mut events, id, name).await
        }
    }
}

/// The snapshot and queue `link::connect` asks for, in whichever order.
async fn state(
    events: &mut UnboundedReceiver<Incoming>,
) -> Result<(Snapshot, (Vec<QueueItem>, usize))> {
    let (mut snapshot, mut queue) = (None, None);
    while snapshot.is_none() || queue.is_none() {
        match events.recv().await {
            Some(Incoming::Event(event)) => match *event {
                Event::Snapshot(state) => snapshot = Some(state),
                Event::Queue { items, position } => queue = Some((items, position)),
                _ => {}
            },
            Some(Incoming::Lost(why)) => bail!("Lost slipmatd: {why}"),
            None => bail!("Lost slipmatd."),
        }
    }
    Ok((snapshot.context("no snapshot")?, queue.context("no queue")?))
}

async fn check(
    link: &Link,
    events: &mut UnboundedReceiver<Incoming>,
    id: String,
    name: Option<String>,
) -> Result<()> {
    eprintln!(
        "Checking {id}{}: downloading and decoding the whole track without sound.",
        name.map(|name| format!(" ({name})")).unwrap_or_default()
    );
    eprintln!("Playback stops; the track restarts from its beginning when you play it again.");
    let started = Instant::now();
    link.send(Request::Check { id: id.clone() });
    loop {
        let event = match events.recv().await {
            Some(Incoming::Event(event)) => event,
            Some(Incoming::Lost(why)) => bail!("Lost slipmatd during the check: {why}"),
            None => bail!("Lost slipmatd during the check."),
        };
        let Event::Checked {
            id: checked,
            played,
            outcome,
            detail,
        } = *event
        else {
            continue;
        };
        if checked != id {
            continue;
        }
        println!("{outcome}");
        if let Some(detail) = detail {
            println!("{detail}");
        }
        eprintln!("({} s)", started.elapsed().as_secs());
        return if played {
            Ok(())
        } else {
            Err(anyhow!("The track did not play through."))
        };
    }
}

/// The track's name, if the daemon is holding it anywhere.
fn name_of(id: &str, snapshot: &Snapshot, queue: &[QueueItem]) -> Option<String> {
    if snapshot.track_id.as_deref() == Some(id) {
        return Some(describe(&snapshot.title, &snapshot.artist));
    }
    queue
        .iter()
        .find(|item| item.id.as_deref() == Some(id))
        .map(|item| describe(&item.title, &item.artist))
}

fn describe(title: &str, artist: &str) -> String {
    if artist.is_empty() {
        title.to_owned()
    } else {
        format!("{title} — {artist}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Command>> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        Command::parse(&args)
    }

    #[test]
    fn commands_parse_and_anything_else_falls_through_to_the_player() {
        assert!(matches!(parse(&["current"]), Ok(Some(Command::Current))));
        assert!(matches!(parse(&["queue"]), Ok(Some(Command::Queue))));
        assert!(matches!(parse(&["check"]), Ok(Some(Command::Check(None)))));
        assert!(matches!(
            parse(&["check", "693269578"]),
            Ok(Some(Command::Check(Some(id)))) if id == "693269578"
        ));
        assert!(parse(&["check", "1", "2"]).is_err());
        assert!(matches!(parse(&[]), Ok(None)));
        assert!(matches!(parse(&["sign-in"]), Ok(None)));
    }
}

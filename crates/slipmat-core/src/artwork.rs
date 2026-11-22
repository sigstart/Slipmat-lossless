// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Album art: fetch once, keep on disk.
//!
//! Two reasons this caches to a file rather than holding bytes in memory:
//!
//! 1. **MPRIS needs a path.** `mpris:artUrl` has to be a `file://` URL — the
//!    GNOME Shell applet will not reliably fetch an `https://` one.
//! 2. Apple serves artwork as a *template* (`…/{w}x{h}bb.jpg`), so we request
//!    exactly the pixels the caller needs instead of scaling a 3000px JPEG.
//!
//! Decoding is not here. Turning a JPEG into pixels is a toolkit's job and a
//! terminal wants none of it — but every frontend, and the daemon, needs the
//! same file in the same place.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::music::types::Artwork;
use crate::paths::artwork_dir;

/// The size fetched for a now-playing bar and for MPRIS. One size keeps the
/// cache simple; a Shell scales it down and nobody notices.
pub const ART_SIZE: u32 = 512;

// Leave room for short motion-art video assets as well as static images.
const MAX_ARTWORK_BYTES: usize = 64 * 1024 * 1024;
static DOWNLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
static HTTP: tokio::sync::OnceCell<reqwest::Client> = tokio::sync::OnceCell::const_new();

fn allowed_artwork_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| {
            host == "mzstatic.com"
                    || host.ends_with(".mzstatic.com")
                    // Covers of tracks uploaded to iCloud Music Library.
                    || host.ends_with(".blobstore.apple.com")
        })
}

/// Where a cover of this size lands on disk.
pub fn cache_path(art: &Artwork, size: u32) -> Option<PathBuf> {
    Some(artwork_dir()?.join(format!("{}-{size}.jpg", art.cache_key())))
}

/// surfacing an error, because a missing cover is not worth a toast.
pub async fn fetch(art: Artwork, size: u32) -> Result<PathBuf> {
    let path = cache_path(&art, size).context("no cache directory available")?;
    if path.is_file() {
        return Ok(path);
    }

    let url = reqwest::Url::parse(&art.url(size)).context("invalid artwork URL")?;
    anyhow::ensure!(allowed_artwork_url(&url), "untrusted artwork URL");
    let _permit = DOWNLOADS.acquire().await?;
    if path.is_file() {
        return Ok(path);
    }
    let client = HTTP
        .get_or_try_init(|| async {
            reqwest::Client::builder()
                .https_only(true)
                .timeout(std::time::Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if attempt.previous().len() >= 5 || !allowed_artwork_url(attempt.url()) {
                        return attempt.error("untrusted artwork redirect or too many redirects");
                    }
                    attempt.follow()
                }))
                .build()
        })
        .await?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_ARTWORK_BYTES as u64),
        "artwork exceeds download limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            chunk.len() <= MAX_ARTWORK_BYTES - bytes.len(),
            "artwork exceeds download limit"
        );
        bytes.extend_from_slice(&chunk);
    }

    write_atomically(&path, &bytes)?;
    Ok(path)
}

/// `rename` within the same directory is atomic.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_urls_are_confined_to_the_apple_cdn() {
        for (url, allowed) in [
            ("https://is1.mzstatic.com/cover.jpg", true),
            (
                "https://store-033.blobstore.apple.com/sq-mq-us-033/image?X-Amz-Signature=a",
                true,
            ),
            ("https://blobstore.apple.com.example.org/image", false),
            ("https://mvod.itunes.apple.com/preview.mp4", false),
            ("https://mzstatic.com.example.org/cover.jpg", false),
            ("http://is1.mzstatic.com/cover.jpg", false),
            ("https://is1.mzstatic.com:8443/cover.jpg", false),
        ] {
            assert_eq!(
                allowed_artwork_url(&reqwest::Url::parse(url).unwrap()),
                allowed
            );
        }
    }

    #[test]
    fn a_failed_atomic_write_keeps_the_destination() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "slipmat-atomic-write-{}-{unique}",
            std::process::id()
        ));
        let path = root.join("snapshot.json");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&path, "previous complete snapshot").unwrap();
        std::fs::create_dir(path.with_extension(format!("tmp{}", std::process::id()))).unwrap();

        assert!(write_atomically(&path, b"replacement").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "previous complete snapshot"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}

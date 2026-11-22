// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Apple Music's catalogue, as the provider needs it: the credentials a
//! signed-in session hands the daemon, and a track's lossless asset.
//!
//! Tokens leave the sandbox only in the `signedIn` reply, and never appear in
//! an error: a failed lookup is reported by its stage and nothing more.

use std::time::Duration;

use anyhow::{Result, bail};
use fairplay_wire::{Catalogue, PlaybackStage};
use reqwest::{StatusCode, Url};
use serde::Deserialize;

use crate::fairplay::Fairplay;

const STOREFRONT: &str = "https://api.music.apple.com/v1/me/storefront";
/// The catalogue host that answers `extendedAssetUrls`, as the web player asks it.
const ASSETS: &str = "https://amp-api.music.apple.com/v1/catalog";
const MAX_RESPONSE: usize = 1 << 20;

pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// The track's `enhancedHls` master playlist, or the stage that failed.
pub async fn lossless_asset(
    client: &reqwest::Client,
    catalogue: &Catalogue,
    track_id: &str,
) -> Result<Url, PlaybackStage> {
    let response = client
        .get(format!(
            "{ASSETS}/{}/songs/{track_id}?extend=extendedAssetUrls",
            catalogue.storefront
        ))
        .bearer_auth(&catalogue.developer_token)
        .header("Origin", "https://music.apple.com")
        .send()
        .await
        .map_err(|_| PlaybackStage::Catalogue)?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return Err(PlaybackStage::CatalogueAuthorization);
        }
        StatusCode::NOT_FOUND => return Err(PlaybackStage::CatalogueTrack),
        _ => return Err(PlaybackStage::Catalogue),
    }
    let body = response
        .bytes()
        .await
        .map_err(|_| PlaybackStage::Catalogue)?;
    if body.len() > MAX_RESPONSE {
        return Err(PlaybackStage::Catalogue);
    }
    enhanced_hls(&body, track_id)
}

fn enhanced_hls(body: &[u8], track_id: &str) -> Result<Url, PlaybackStage> {
    #[derive(Deserialize)]
    struct Songs {
        data: Vec<Song>,
    }
    #[derive(Deserialize)]
    struct Song {
        id: String,
        #[serde(default)]
        attributes: Attributes,
    }
    #[derive(Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Attributes {
        #[serde(default)]
        extended_asset_urls: Assets,
    }
    #[derive(Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Assets {
        enhanced_hls: Option<String>,
    }
    let songs: Songs = serde_json::from_slice(body).map_err(|_| PlaybackStage::Catalogue)?;
    let [song] = songs.data.as_slice() else {
        return Err(PlaybackStage::CatalogueTrack);
    };
    if song.id != track_id {
        return Err(PlaybackStage::CatalogueTrack);
    }
    song.attributes
        .extended_asset_urls
        .enhanced_hls
        .as_deref()
        .and_then(|asset| Url::parse(asset).ok())
        .filter(|url| url.scheme() == "https" && url.host_str().is_some())
        .ok_or(PlaybackStage::CatalogueAsset)
}

pub async fn credentials(fairplay: &Fairplay) -> Result<Catalogue> {
    let account = fairplay.account().await?;
    let storefront = storefront(&account.developer_token, &account.music_user_token).await?;
    Ok(Catalogue {
        developer_token: account.developer_token,
        music_user_token: account.music_user_token,
        storefront,
        authorized: true,
    })
}

async fn storefront(developer_token: &str, music_user_token: &str) -> Result<String> {
    let response = client()?
        .get(STOREFRONT)
        .bearer_auth(developer_token)
        .header("Music-User-Token", music_user_token)
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::OK {
        bail!("the storefront lookup was refused");
    }
    only_storefront(&response.bytes().await?)
}

fn only_storefront(body: &[u8]) -> Result<String> {
    #[derive(Deserialize)]
    struct Regions {
        data: Vec<Region>,
    }
    #[derive(Deserialize)]
    struct Region {
        id: String,
    }
    let Ok(Regions { data }) = serde_json::from_slice(body) else {
        bail!("unreadable storefront");
    };
    match data.as_slice() {
        [region] if is_storefront_code(&region.id) => Ok(region.id.clone()),
        _ => bail!("the account has no single storefront"),
    }
}

fn is_storefront_code(code: &str) -> bool {
    code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_two_letter_storefront_is_accepted() {
        assert_eq!(
            only_storefront(br#"{"data":[{"id":"za","type":"storefronts"}]}"#).unwrap(),
            "za"
        );
        for body in [
            &br#"{"data":[]}"#[..],
            br#"{"data":[{"id":"za"},{"id":"us"}]}"#,
            br#"{"data":[{"id":"ZA"}]}"#,
            br#"{"data":[{"id":"zaf"}]}"#,
            b"<html>",
        ] {
            assert!(only_storefront(body).is_err());
        }
    }
}

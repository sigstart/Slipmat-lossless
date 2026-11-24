// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-FileCopyrightText: 2026 Jan Vermeulen
// SPDX-License-Identifier: GPL-3.0-or-later

//! Apple Music's catalogue, as the provider needs it: the credentials a
//! signed-in session hands the daemon, and where a track's audio is.
//!
//! A track with a lossless master has an `enhancedHls` asset. One without has
//! only legacy `.m4p` files under the old iTunes DRM, which this provider
//! cannot open, so its stream comes from `webPlayback` instead: the 256 kbps
//! AAC flavor under Widevine, licensed through Google's CDM.
//!
//! Its FairPlay twin (`cbcp256`) exists too, but its keys are licensed by the
//! web player's server, which answers Apple's Android library with HTTP 500:
//! measured 2026-11-24, and why this path is Widevine.
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
/// Where Apple's player asks for a song's streams by flavor.
const WEB_PLAYBACK: &str = "https://play.music.apple.com/WebObjects/MZPlay.woa/wa/webPlayback";
/// The web player's licence server, Widevine's included.
const WEB_LICENSE: &str =
    "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/acquireWebPlaybackLicense";
/// 256 kbps AAC-LC under Widevine. Not `ibhp` (a key handed out over HTTPS,
/// which would put it outside the DRM component), and not the 64 kbps
/// flavors, which are HE-AAC.
const AAC_FLAVOR: &str = "28:ctrp256";
/// `webPlayback` answers `5002` without one, measured 2026-11-24.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36";
const MAX_RESPONSE: usize = 1 << 20;

pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// Where a track's audio comes from.
pub enum Source {
    /// An `enhancedHls` master playlist, which lists ALAC and AAC variants.
    Master(Url),
    /// A media playlist of Widevine AAC, and the licence server's certificate.
    Widevine { playlist: Url, certificate: Url },
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

/// The Widevine AAC stream `webPlayback` offers a song with no lossless master.
pub async fn web_playback_aac(
    client: &reqwest::Client,
    catalogue: &Catalogue,
    track_id: &str,
) -> Result<Source, PlaybackStage> {
    let mut body = serde_json::json!({ "salableAdamId": track_id });
    if catalogue.storefront != "us" {
        body["country"] = catalogue.storefront.to_uppercase().into();
    }
    let response = web_player(client.post(WEB_PLAYBACK), catalogue)
        .json(&body)
        .send()
        .await
        .map_err(|_| PlaybackStage::Catalogue)?;
    aac_flavor(&answer(response).await?)
}

/// A Widevine licence for one CDM message, from Apple's web licence server.
/// The message and the licence are the CDM's: opaque here, and never logged.
pub async fn widevine_license(
    client: &reqwest::Client,
    catalogue: &Catalogue,
    track_id: &str,
    key_uri: &str,
    challenge: &[u8],
) -> Result<Vec<u8>, PlaybackStage> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    let body = serde_json::json!({
        "challenge": BASE64.encode(challenge),
        "key-system": "com.widevine.alpha",
        "uri": key_uri,
        "adamId": track_id,
        "isLibrary": false,
        "user-initiated": true,
    });
    let response = web_player(client.post(WEB_LICENSE), catalogue)
        .json(&body)
        .send()
        .await
        .map_err(|_| PlaybackStage::Catalogue)?;
    #[derive(Deserialize)]
    struct Licence {
        #[serde(default)]
        license: String,
        #[serde(default, rename = "errorCode")]
        error_code: i64,
        #[serde(default)]
        status: i64,
    }
    let licence: Licence =
        serde_json::from_slice(&answer(response).await?).map_err(|_| PlaybackStage::Catalogue)?;
    if licence.error_code != 0 || licence.status != 0 || licence.license.is_empty() {
        return Err(PlaybackStage::CatalogueAuthorization);
    }
    BASE64
        .decode(licence.license)
        .map_err(|_| PlaybackStage::Catalogue)
}

/// The headers Apple's web endpoints want.
fn web_player(request: reqwest::RequestBuilder, catalogue: &Catalogue) -> reqwest::RequestBuilder {
    request
        .bearer_auth(&catalogue.developer_token)
        .header("x-apple-music-user-token", &catalogue.music_user_token)
        .header("Origin", "https://music.apple.com")
        .header("User-Agent", USER_AGENT)
}

async fn answer(response: reqwest::Response) -> Result<Vec<u8>, PlaybackStage> {
    match response.status() {
        StatusCode::OK => {}
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return Err(PlaybackStage::CatalogueAuthorization);
        }
        _ => return Err(PlaybackStage::Catalogue),
    }
    let body = response
        .bytes()
        .await
        .map_err(|_| PlaybackStage::Catalogue)?;
    if body.len() > MAX_RESPONSE {
        return Err(PlaybackStage::Catalogue);
    }
    Ok(body.to_vec())
}

/// The Widevine AAC flavor and the certificate, from a `webPlayback` answer.
fn aac_flavor(body: &[u8]) -> Result<Source, PlaybackStage> {
    #[derive(Deserialize)]
    struct Answer {
        #[serde(default, rename = "songList")]
        songs: Vec<Song>,
    }
    #[derive(Deserialize)]
    struct Song {
        #[serde(default)]
        assets: Vec<Flavor>,
        #[serde(default, rename = "widevine-cert-url")]
        certificate: Option<String>,
    }
    #[derive(Deserialize)]
    struct Flavor {
        flavor: String,
        #[serde(rename = "URL")]
        url: String,
    }
    let https = |url: &str| {
        Url::parse(url)
            .ok()
            .filter(|url| url.scheme() == "https" && url.host_str().is_some())
    };
    let answer: Answer = serde_json::from_slice(body).map_err(|_| PlaybackStage::Catalogue)?;
    let song = answer.songs.first().ok_or(PlaybackStage::CatalogueTrack)?;
    let playlist = song
        .assets
        .iter()
        .find(|asset| asset.flavor == AAC_FLAVOR)
        .and_then(|asset| https(&asset.url))
        .ok_or(PlaybackStage::CatalogueAsset)?;
    let certificate = song
        .certificate
        .as_deref()
        .and_then(https)
        .ok_or(PlaybackStage::CatalogueAsset)?;
    Ok(Source::Widevine {
        playlist,
        certificate,
    })
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

    #[test]
    fn the_widevine_aac_flavor_is_chosen_over_the_others() {
        let answer =
            br#"{"songList":[{"widevine-cert-url":"https://aod.invalid/cert.bin","assets":[
            {"flavor":"30:cbcp256","URL":"https://aod.invalid/cbcs.m3u8"},
            {"flavor":"37:ibhp256","URL":"https://aod.invalid/ibhp.m3u8"},
            {"flavor":"28:ctrp256","URL":"https://aod.invalid/ctr.m3u8"},
            {"flavor":"32:ctrp64","URL":"https://aod.invalid/he.m3u8"}]}]}"#;
        let Ok(Source::Widevine {
            playlist,
            certificate,
        }) = aac_flavor(answer)
        else {
            panic!("no Widevine source");
        };
        assert_eq!(playlist.as_str(), "https://aod.invalid/ctr.m3u8");
        assert_eq!(certificate.as_str(), "https://aod.invalid/cert.bin");
        let fairplay_only =
            br#"{"songList":[{"widevine-cert-url":"https://aod.invalid/c","assets":[
            {"flavor":"30:cbcp256","URL":"https://aod.invalid/cbcs.m3u8"}]}]}"#;
        assert!(matches!(
            aac_flavor(fairplay_only),
            Err(PlaybackStage::CatalogueAsset)
        ));
        let no_certificate = br#"{"songList":[{"assets":[
            {"flavor":"28:ctrp256","URL":"https://aod.invalid/ctr.m3u8"}]}]}"#;
        assert!(matches!(
            aac_flavor(no_certificate),
            Err(PlaybackStage::CatalogueAsset)
        ));
        assert!(matches!(
            aac_flavor(br#"{"failureType":"5002"}"#),
            Err(PlaybackStage::CatalogueTrack)
        ));
    }
}

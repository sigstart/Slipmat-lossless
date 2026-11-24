// SPDX-FileCopyrightText: 2026 Jan Vermeulen
// SPDX-License-Identifier: GPL-3.0-or-later

//! Google's Widevine CDM, for the AAC of songs Apple has no lossless master
//! for. `wvhost` hosts it the way a browser does, one process per track,
//! inside this provider's sandbox; this is the provider's end of its pipes.
//!
//! The CDM keeps its keys. What crosses the pipe is the CDM's own licence
//! messages, which only Apple's server and the CDM can read, and samples:
//! encrypted in, clear out, in memory, as the FairPlay path decrypts them.
//! The catalogue credentials never reach the host: the licence exchange is
//! the caller's, passed in as a closure.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use crate::mp4::Fragment;

const HOST: &str = "/app/wvhost";
const CDM: &str = "/app/widevine/libwidevinecdm.so";
const STATE: &str = "/state/widevine";
/// Widevine's system id, as a PSSH box names it.
const SYSTEM_ID: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed,
];
/// Loading the CDM, or one licence step, takes well under a second.
const STEP: Duration = Duration::from_secs(20);
/// `CLEAR`'s success status, cdm::kSuccess.
const CLEAR_OK: &str = "0";

/// Whether `make widevine` has put the CDM where the sandbox binds it.
pub fn installed() -> bool {
    Path::new(CDM).is_file()
}

pub struct Widevine {
    _child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
}

impl Widevine {
    pub async fn start() -> Result<Self> {
        let mut child = Command::new(HOST)
            .arg(CDM)
            .arg(STATE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting wvhost")?;
        let input = child.stdin.take().context("no wvhost stdin")?;
        let output = BufReader::new(child.stdout.take().context("no wvhost stdout")?).lines();
        let mut host = Self {
            _child: child,
            input,
            output,
        };
        host.wait_ready().await?;
        Ok(host)
    }

    async fn wait_ready(&mut self) -> Result<()> {
        loop {
            let line = self.line().await?;
            match line.as_str() {
                "READY" => return Ok(()),
                "INITFAIL" => bail!("the Widevine CDM did not initialise"),
                other => log(other),
            }
        }
    }

    /// Opens a temporary session for one key and licenses it: `exchange`
    /// takes each message the CDM sends and returns Apple's answer.
    pub async fn license<F, A>(
        &mut self,
        certificate: &[u8],
        key_id: &[u8; 16],
        mut exchange: F,
    ) -> Result<()>
    where
        F: FnMut(Vec<u8>) -> A,
        A: Future<Output = Result<Vec<u8>>>,
    {
        self.send("CERT", certificate).await?;
        self.send("PSSH", &pssh(key_id)).await?;
        let wanted = format!("{}:", hex(key_id));
        loop {
            let line = self.line().await?;
            let mut words = line.split(' ');
            match words.next().unwrap_or_default() {
                "MSG" => {
                    let kind = words.next().unwrap_or_default();
                    let message = BASE64
                        .decode(words.next().unwrap_or_default())
                        .context("unreadable CDM message")?;
                    match kind {
                        // A licence request, or its renewal.
                        "0" | "1" => {
                            let license = exchange(message).await?;
                            self.send("UPDATE", &license).await?;
                        }
                        "3" => bail!("the Widevine CDM asked to be individualised"),
                        other => bail!("unexpected CDM message type {other}"),
                    }
                }
                "KEYS" => {
                    let status = words
                        .find_map(|key| key.strip_prefix(&wanted))
                        .context("the licence did not include the track's key")?;
                    // cdm::kUsable.
                    ensure!(
                        status == "0",
                        "the track's key is not usable (status {status})"
                    );
                    return Ok(());
                }
                "PROMISE" if line.contains(" reject ") => bail!("the Widevine CDM refused: {line}"),
                _ => log(&line),
            }
        }
    }

    /// Decrypts every sample of a cenc fragment in place.
    pub async fn decrypt(&mut self, key_id: &[u8; 16], fragment: &mut Fragment) -> Result<()> {
        let kid = BASE64.encode(key_id);
        for sample in &fragment.samples {
            let data = fragment
                .bytes
                .get_mut(sample.range.clone())
                .context("sample outside its fragment")?;
            let map = if sample.map.is_empty() {
                "-".to_owned()
            } else {
                sample
                    .map
                    .iter()
                    .map(|(clear, cipher)| format!("{clear}:{cipher}"))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let request = format!(
                "DECRYPT {kid} {} {map} {}\n",
                BASE64.encode(&sample.iv),
                BASE64.encode(&*data)
            );
            self.input
                .write_all(request.as_bytes())
                .await
                .context("writing to wvhost")?;
            let line = loop {
                let line = self.line().await?;
                if line.starts_with("CLEAR ") {
                    break line;
                }
                log(&line);
            };
            let mut words = line.split(' ').skip(1);
            let status = words.next().unwrap_or_default();
            ensure!(
                status == CLEAR_OK,
                "the CDM refused a sample (status {status})"
            );
            let clear = BASE64
                .decode(words.next().unwrap_or_default())
                .context("unreadable clear sample")?;
            ensure!(clear.len() == data.len(), "the CDM changed a sample's size");
            data.copy_from_slice(&clear);
        }
        Ok(())
    }

    async fn send(&mut self, verb: &str, bytes: &[u8]) -> Result<()> {
        self.input
            .write_all(format!("{verb} {}\n", BASE64.encode(bytes)).as_bytes())
            .await
            .context("writing to wvhost")
    }

    async fn line(&mut self) -> Result<String> {
        timeout(STEP, self.output.next_line())
            .await
            .context("wvhost stopped answering")??
            .context("wvhost exited")
    }
}

/// The host's own notes; nothing in them is key material.
fn log(line: &str) {
    if let Some(note) = line.strip_prefix("LOG ") {
        eprintln!("[widevine] {note}");
    }
}

/// A `cenc` PSSH box for one key: Widevine's protobuf with the key id and
/// AES-CTR, and empty provider, content id and policy, as Apple's player sends.
pub fn pssh(key_id: &[u8; 16]) -> Vec<u8> {
    let mut data = vec![0x08, 0x01, 0x12, 16];
    data.extend_from_slice(key_id);
    data.extend_from_slice(&[0x1a, 0x00, 0x22, 0x00, 0x32, 0x00]);
    let mut boxed = Vec::with_capacity(32 + data.len());
    boxed.extend_from_slice(&(32 + data.len() as u32).to_be_bytes());
    boxed.extend_from_slice(b"pssh");
    boxed.extend_from_slice(&[0; 4]);
    boxed.extend_from_slice(&SYSTEM_ID);
    boxed.extend_from_slice(&(data.len() as u32).to_be_bytes());
    boxed.extend_from_slice(&data);
    boxed
}

/// The key id from a `data:;base64,<id>` key URI: its first 16 bytes, since
/// Apple sometimes appends to it.
pub fn key_id(uri: &str) -> Result<[u8; 16]> {
    let encoded = uri.rsplit(',').next().context("key URI without data")?;
    let bytes = BASE64.decode(encoded).context("key URI is not base64")?;
    bytes
        .get(..16)
        .and_then(|id| id.try_into().ok())
        .with_context(|| format!("key id of {} bytes", bytes.len()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pssh_box_is_sized_and_names_widevine() {
        let boxed = pssh(&[7; 16]);
        assert_eq!(
            u32::from_be_bytes(boxed[..4].try_into().unwrap()) as usize,
            boxed.len()
        );
        assert_eq!(&boxed[4..8], b"pssh");
        assert_eq!(&boxed[12..28], &SYSTEM_ID);
        assert_eq!(&boxed[32..36], &[0x08, 0x01, 0x12, 16]);
        assert_eq!(&boxed[36..52], &[7; 16]);
    }

    #[test]
    fn the_key_id_is_the_first_16_bytes_of_the_uri_data() {
        let uri = format!("data:;base64,{}", BASE64.encode([9; 24]));
        assert_eq!(key_id(&uri).unwrap(), [9; 16]);
        assert!(key_id("data:;base64,AAAA").is_err());
        assert!(key_id("data:;base64,!!").is_err());
    }
}

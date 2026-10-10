use anyhow::{Context, Result};
use std::time::Duration;

/// Client for the wrapper-lite single-port HTTP API.
///
/// All endpoints answer `{"code":0,"msg":"SUCCESS","data":...}`; non-zero
/// `code` is an error carrying `msg`.
#[derive(Clone)]
pub struct Wrapper {
    base: String,
    agent: ureq::Agent,
}

#[derive(Debug, serde::Deserialize)]
struct Envelope {
    code: i64,
    msg: String,
    #[serde(default = "serde_json::Value::default")]
    data: serde_json::Value,
}

impl Wrapper {
    pub fn new(base: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout(Duration::from_secs(30))
            .build();
        Self {
            base: base.trim_end_matches('/').to_string(),
            agent,
        }
    }

    /// One decoded envelope, non-zero `code` included (callers that treat a
    /// specific code as data — e.g. `/lyrics` 404 = "no lyrics" — need it).
    fn envelope(&self, path: &str, queries: &[(&str, &str)]) -> Result<Envelope> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.get(&url);
        for (k, v) in queries {
            req = req.query(k, v);
        }
        req.call()
            .with_context(|| format!("wrapper GET {path}"))?
            .into_json()
            .with_context(|| format!("wrapper GET {path} body"))
    }

    fn get(&self, path: &str, queries: &[(&str, &str)]) -> Result<serde_json::Value> {
        let env = self.envelope(path, queries)?;
        if env.code != 0 {
            anyhow::bail!("wrapper {path} failed (code {}): {}", env.code, env.msg);
        }
        Ok(env.data)
    }

    /// Storefront codes this wrapper can serve (empty = not logged in).
    pub fn regions(&self) -> Result<Vec<String>> {
        let data = self.get("/status", &[])?;
        Ok(data
            .get("regions")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Playback manifest URL for a track (adam id). wrapper-lite returns the
    /// hosted m3u8 URL rather than manifest text.
    pub fn m3u8_url(&self, adam_id: &str) -> Result<String> {
        let data = self.get("/m3u8", &[("adamId", adam_id)])?;
        data.get("m3u8")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("unexpected /m3u8 data shape: {data}"))
    }

    /// Decryption key bytes for an `skd://` URI of a track.
    pub fn key(&self, adam_id: &str, uri: &str) -> Result<serde_json::Value> {
        self.get("/key", &[("adamId", adam_id), ("uri", uri)])
    }

    /// Time-synced lyrics; `syllable` selects word-by-word timing.
    pub fn lyrics(&self, adam_id: &str, syllable: bool) -> Result<serde_json::Value> {
        self.get(
            "/lyrics",
            &[("adamId", adam_id), ("syllable", if syllable { "1" } else { "0" })],
        )
    }

    /// Lyrics **TTML** for a track (`data.lyrics` of `/lyrics`). `Ok(None)` is
    /// the wrapper's own "no lyrics for this track" answer (envelope code 404,
    /// `{"code":404,"msg":"lyrics not found"}`); `Err` is either a transport
    /// failure (wrapper down / offline) or any other upstream error code.
    pub fn lyrics_ttml(&self, adam_id: &str, syllable: bool) -> Result<Option<String>> {
        let env = self.envelope(
            "/lyrics",
            &[("adamId", adam_id), ("syllable", if syllable { "1" } else { "0" })],
        )?;
        if env.code == 404 {
            return Ok(None);
        }
        if env.code != 0 {
            anyhow::bail!("wrapper /lyrics failed (code {}): {}", env.code, env.msg);
        }
        Ok(env
            .data
            .get("lyrics")
            .and_then(|v| v.as_str())
            .map(str::to_string))
    }
}

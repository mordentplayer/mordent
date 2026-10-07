use crate::config::{cache_path, fresh, Config};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::time::Duration;

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

// ---- public types ----

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub track_no: u32,
    pub disc_no: u32,
    pub artwork: Option<String>,
    pub lossless: bool,
}

#[derive(Clone, Debug)]
pub struct AlbumHit {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub artwork: Option<String>,
    pub track_count: u32,
    pub release_date: String,
}

#[derive(Clone, Debug)]
pub struct ArtistHit {
    pub id: String,
    pub name: String,
    pub genres: String,
}

#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    pub tracks: Vec<Track>,
    pub albums: Vec<AlbumHit>,
    pub artists: Vec<ArtistHit>,
}

// ---- raw AMP-API JSON shapes (camelCase wire format) ----

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawAttrs {
    name: String,
    artist_name: String,
    album_name: String,
    duration_in_millis: u64,
    track_number: u32,
    disc_number: u32,
    artwork: Option<RawArtwork>,
    audio_traits: Vec<String>,
    #[serde(default)]
    previews: Vec<RawPreview>,
    track_count: u32,
    release_date: String,
    genre_names: Vec<String>,
}

#[derive(Deserialize, Default)]
struct RawArtwork {
    url: String,
}

#[derive(Deserialize, Default)]
struct RawPreview {
    url: String,
}

#[derive(Deserialize)]
struct RawResource {
    id: String,
    #[serde(default)]
    attributes: RawAttrs,
}

impl RawResource {
    fn artwork_templated(&self) -> Option<String> {
        self.attributes
            .artwork
            .as_ref()
            .map(|a| a.url.replace("{w}x{h}bb.jpg", "600x600bb.jpg"))
    }
}

#[derive(Deserialize, Default)]
struct RawGroup {
    #[serde(default)]
    data: Vec<RawResource>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawSearchResults {
    songs: RawGroup,
    albums: RawGroup,
    artists: RawGroup,
}

#[derive(Deserialize)]
struct RawSearch {
    results: RawSearchResults,
}

// ---- client ----

pub struct Api {
    agent: ureq::Agent,
    pub cfg: Config,
    bearer: RefCell<Option<String>>,
}

impl Api {
    pub fn new(cfg: Config) -> Result<Self> {
        cfg.ensure_cache_dir()?;
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .user_agent(UA)
            .build();
        Ok(Self {
            agent,
            cfg,
            bearer: RefCell::new(None),
        })
    }

    /// Bearer JWT scraped from the web player, cached on disk ~12h.
    pub fn bearer(&self) -> Result<String> {
        if let Some(t) = self.bearer.borrow().as_ref() {
            return Ok(t.clone());
        }
        let cache = cache_path(&self.cfg, "bearer.jwt")?;
        if fresh(&cache, 12 * 3600) {
            if let Ok(t) = std::fs::read_to_string(&cache) {
                let t = t.trim().to_string();
                if !t.is_empty() {
                    *self.bearer.borrow_mut() = Some(t.clone());
                    return Ok(t);
                }
            }
        }
        let t = scrape_bearer(&self.agent, &self.cfg.storefront)?;
        std::fs::write(&cache, &t).context("caching bearer token")?;
        *self.bearer.borrow_mut() = Some(t.clone());
        Ok(t)
    }

    /// Invalidate cached bearer and re-scrape (on 401).
    fn refresh_bearer(&self) -> Result<String> {
        *self.bearer.borrow_mut() = None;
        let cache = cache_path(&self.cfg, "bearer.jwt")?;
        let _ = std::fs::remove_file(&cache);
        self.bearer()
    }

    fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        let bearer = self.bearer()?;
        let mut req = self
            .agent
            .get(url)
            .set("Authorization", &format!("Bearer {bearer}"))
            .set("Origin", "https://music.apple.com");
        if let Some(t) = &self.cfg.media_user_token {
            if !t.is_empty() {
                req = req.set("Media-User-Token", t);
            }
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        resp.into_json().with_context(|| format!("body of {url}"))
    }

    /// GET with a single transparent 401-retry after bearer refresh.
    fn get_json_retry(&self, url: &str) -> Result<serde_json::Value> {
        match self.get_json(url) {
            Ok(v) => Ok(v),
            Err(e) => {
                if is_unauthorized(&e) {
                    self.refresh_bearer()?;
                    self.get_json(url)
                } else {
                    Err(e)
                }
            }
        }
    }

    pub fn search(&self, term: &str, limit: u32) -> Result<SearchResults> {
        let url = format!(
            "https://amp-api.music.apple.com/v1/catalog/{}/search?term={}&limit={}&types=songs,albums,artists&l=en-US",
            self.cfg.storefront,
            urlencode(term),
            limit
        );
        let raw: RawSearch =
            from_value(self.get_json_retry(&url)?).context("decoding search response")?;
        Ok(SearchResults {
            tracks: raw
                .results
                .songs
                .data
                .iter()
                .map(|r| Track {
                    id: r.id.clone(),
                    name: r.attributes.name.clone(),
                    artist: r.attributes.artist_name.clone(),
                    album: r.attributes.album_name.clone(),
                    duration_ms: r.attributes.duration_in_millis,
                    track_no: r.attributes.track_number,
                    disc_no: r.attributes.disc_number,
                    artwork: r.artwork_templated(),
                    lossless: r
                        .attributes
                        .audio_traits
                        .iter()
                        .any(|t| t == "lossless" || t == "hi-res-lossless"),
                })
                .collect(),
            albums: raw
                .results
                .albums
                .data
                .iter()
                .map(|r| AlbumHit {
                    id: r.id.clone(),
                    name: r.attributes.name.clone(),
                    artist: r.attributes.artist_name.clone(),
                    artwork: r.artwork_templated(),
                    track_count: r.attributes.track_count,
                    release_date: r.attributes.release_date.clone(),
                })
                .collect(),
            artists: raw
                .results
                .artists
                .data
                .iter()
                .map(|r| ArtistHit {
                    id: r.id.clone(),
                    name: r.attributes.name.clone(),
                    genres: r.attributes.genre_names.join(", "),
                })
                .collect(),
        })
    }

    /// 90-second unencrypted preview URL for a track (anonymous playback).
    pub fn preview_url(&self, id: &str) -> Result<String> {
        let url = format!(
            "https://amp-api.music.apple.com/v1/catalog/{}/songs/{id}?l=en-US",
            self.cfg.storefront
        );
        let raw = self.get_json_retry(&url)?;
        raw.get("data")
            .and_then(|d| d.get(0))
            .and_then(|r| r.get("attributes"))
            .and_then(|a| a.get("previews"))
            .and_then(|p| p.get(0))
            .and_then(|p| p.get("url"))
            .and_then(|u| u.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("no preview for {id}"))
    }

    /// Tracks of a catalog album, in disc/track order.
    pub fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        let url = format!(
            "https://amp-api.music.apple.com/v1/catalog/{}/albums/{album_id}/tracks?limit=300&l=en-US",
            self.cfg.storefront
        );
        let raw: RawGroup =
            from_value(self.get_json_retry(&url)?).context("decoding album tracks")?;
        Ok(raw
            .data
            .iter()
            .map(|r| Track {
                id: r.id.clone(),
                name: r.attributes.name.clone(),
                artist: r
                    .attributes
                    .artist_name
                    .clone(),
                album: String::new(),
                duration_ms: r.attributes.duration_in_millis,
                track_no: r.attributes.track_number,
                disc_no: r.attributes.disc_number,
                artwork: r.artwork_templated(),
                lossless: r
                    .attributes
                    .audio_traits
                    .iter()
                    .any(|t| t == "lossless" || t == "hi-res-lossless"),
            })
            .collect())
    }
}

// ---- helpers ----

fn from_value<T: DeserializeOwned>(v: serde_json::Value) -> Result<T> {
    serde_json::from_value(v).map_err(Into::into)
}

fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<ureq::Error>()
            .is_some_and(|ue| matches!(ue, ureq::Error::Status(401, _)))
    })
}

fn scrape_bearer(agent: &ureq::Agent, storefront: &str) -> Result<String> {
    let html = agent
        .get(&format!("https://music.apple.com/{storefront}/new"))
        .set("Origin", "https://music.apple.com")
        .call()?
        .into_string()?;
    let js_path = find_between(&html, "/assets/index", "\"")
        .context("no index js asset found on web player page")?;
    let js_url = format!("https://music.apple.com/assets/index{js_path}");
    let js = agent.get(&js_url).call()?.into_string()?;
    find_jwt(&js).context("no bearer JWT found in web player js")
}

fn find_between(hay: &str, start: &str, end: &str) -> Option<String> {
    let i = hay.find(start)?;
    let rest = &hay[i + start.len()..];
    let j = rest.find(end)?;
    Some(rest[..j].to_string())
}

fn find_jwt(js: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = js[from..].find("eyJ") {
        let i = from + rel;
        let tok: String = js[i..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            .collect();
        // JWT = three dot-separated base64url segments; both dots must exist.
        if tok.matches('.').count() == 2 && tok.len() > 100 {
            return Some(tok);
        }
        from = i + 3;
    }
    None
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Extract a catalog id from an Apple Music URL or return the input as-is.
pub fn id_from_arg(arg: &str) -> Result<String> {
    if let Some(id) = arg.split('?').next().unwrap_or(arg).rsplit('/').next() {
        if id.chars().all(|c| c.is_ascii_digit()) && !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    if arg.chars().all(|c| c.is_ascii_digit()) {
        return Ok(arg.to_string());
    }
    anyhow::bail!("cannot parse id from {arg:?}")
}

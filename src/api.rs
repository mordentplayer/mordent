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
    #[serde(default)]
    play_params: Option<RawPlayParams>,
    track_count: u32,
    release_date: String,
    genre_names: Vec<String>,
}

#[derive(Deserialize, Default)]
struct RawPlayParams {
    #[serde(default, rename = "catalogId")]
    catalog_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
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

    /// The playable id: catalog adamId when known (library items carry it in
    /// playParams.catalogId), else the resource id.
    fn playable_id(&self) -> String {
        self.attributes
            .play_params
            .as_ref()
            .and_then(|p| p.catalog_id.clone().or_else(|| p.id.clone()))
            .unwrap_or_else(|| self.id.clone())
    }

    fn to_track(&self) -> Track {
        Track {
            id: self.playable_id(),
            name: self.attributes.name.clone(),
            artist: self.attributes.artist_name.clone(),
            album: self.attributes.album_name.clone(),
            duration_ms: self.attributes.duration_in_millis,
            track_no: self.attributes.track_number,
            disc_no: self.attributes.disc_number,
            artwork: self.artwork_templated(),
            lossless: self
                .attributes
                .audio_traits
                .iter()
                .any(|t| t == "lossless" || t == "hi-res-lossless"),
        }
    }

    fn to_album(&self) -> AlbumHit {
        AlbumHit {
            id: self.playable_id(),
            name: self.attributes.name.clone(),
            artist: self.attributes.artist_name.clone(),
            artwork: self.artwork_templated(),
            track_count: self.attributes.track_count,
            release_date: self.attributes.release_date.clone(),
        }
    }

    fn to_artist(&self) -> ArtistHit {
        ArtistHit {
            id: self.id.clone(),
            name: self.attributes.name.clone(),
            genres: self.attributes.genre_names.join(", "),
        }
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
    #[serde(default, rename = "library-songs")]
    library_songs: RawGroup,
    #[serde(default, rename = "library-albums")]
    library_albums: RawGroup,
    #[serde(default, rename = "library-artists")]
    library_artists: RawGroup,
}

#[derive(Deserialize)]
struct RawSearch {
    results: RawSearchResults,
}

#[derive(Deserialize, Default)]
struct RawLibraryResp {
    #[serde(default)]
    data: Vec<RawResource>,
    #[serde(default)]
    next: Option<String>,
}

fn parse_next_offset(next: &str) -> Option<u32> {
    let i = next.find("offset=")?;
    let rest = &next[i + 7..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
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
            tracks: raw.results.songs.data.iter().map(|r| r.to_track()).collect(),
            albums: raw.results.albums.data.iter().map(|r| r.to_album()).collect(),
            artists: raw.results.artists.data.iter().map(|r| r.to_artist()).collect(),
        })
    }

    /// Library search (requires media-user-token). Falls back cleanly to an
    /// empty result when no token is configured.
    pub fn library_search(&self, term: &str, limit: u32) -> Result<SearchResults> {
        if self.cfg.media_user_token.as_deref().unwrap_or("").is_empty() {
            return Ok(SearchResults::default());
        }
        let url = format!(
            "https://amp-api.music.apple.com/v1/me/library/search?term={}&limit={}&types=library-songs,library-albums,library-artists&l=en-US",
            urlencode(term),
            limit
        );
        let raw: RawSearch =
            from_value(self.get_json_retry(&url)?).context("decoding library search")?;
        Ok(SearchResults {
            tracks: raw
                .results
                .library_songs
                .data
                .iter()
                .chain(raw.results.songs.data.iter())
                .map(|r| r.to_track())
                .collect(),
            albums: raw
                .results
                .library_albums
                .data
                .iter()
                .chain(raw.results.albums.data.iter())
                .map(|r| r.to_album())
                .collect(),
            artists: raw.results.library_artists.data.iter().map(|r| r.to_artist()).collect(),
        })
    }

    /// User library songs, most recently added first. Returns tracks plus the
    /// offset for the next page (None = end).
    pub fn library_songs(&self, limit: u32, offset: u32) -> Result<(Vec<Track>, Option<u32>)> {
        let url = format!(
            "https://amp-api.music.apple.com/v1/me/library/songs?limit={limit}&offset={offset}&l=en-US"
        );
        let raw: RawLibraryResp = from_value(self.get_json_retry(&url)?)?;
        let tracks = raw.data.iter().map(|r| r.to_track()).collect();
        Ok((tracks, raw.next.as_deref().and_then(parse_next_offset)))
    }

    /// User library albums (id may be a library id — playable ids resolved
    /// per item via playParams.catalogId).
    pub fn library_albums(&self, limit: u32, offset: u32) -> Result<Vec<AlbumHit>> {
        let url = format!(
            "https://amp-api.music.apple.com/v1/me/library/albums?limit={limit}&offset={offset}&l=en-US"
        );
        let raw: RawLibraryResp = from_value(self.get_json_retry(&url)?)?;
        Ok(raw.data.iter().map(|r| r.to_album()).collect())
    }

    /// Tracks of a library album (id starting with `i.`) or a catalog album.
    pub fn album_tracks_any(&self, id: &str) -> Result<Vec<Track>> {
        if id.starts_with("i.") {
            let url = format!(
                "https://amp-api.music.apple.com/v1/me/library/albums/{id}/tracks?limit=300&l=en-US"
            );
            let raw: RawGroup = from_value(self.get_json_retry(&url)?)?;
            Ok(raw.data.iter().map(|r| r.to_track()).collect())
        } else {
            self.album_tracks(id)
        }
    }

    /// Tracks of a library playlist.
    pub fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let url = format!(
            "https://amp-api.music.apple.com/v1/me/library/playlists/{id}/tracks?limit=300&l=en-US"
        );
        let raw: RawGroup = from_value(self.get_json_retry(&url)?)?;
        Ok(raw.data.iter().map(|r| r.to_track()).collect())
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

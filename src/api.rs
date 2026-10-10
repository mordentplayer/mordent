use crate::config::{cache_path, fnv128, fresh, unix_now, Config};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::path::PathBuf;
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
    /// Concrete 600×600 artwork URL from the catalog payload.
    #[allow(dead_code)] // hit payload field; currently only read by unit tests
    pub artwork: Option<String>,
    pub track_count: u32,
    pub release_date: String,
    /// Library resources only: when the item was added to the library
    /// (ISO-8601). None for catalog results.
    pub date_added: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ArtistHit {
    pub name: String,
    pub genres: String,
    /// Raw artwork object (template + palette metadata) for size-aware renders.
    #[allow(dead_code)] // hit payload field; currently only read by unit tests
    pub artwork_obj: Option<Artwork>,
}

/// Apple artwork object: a URL template (`.../{w}x{h}bb.jpg`) plus palette
/// metadata. Every field is optional/defaulted so partial payloads parse.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Artwork {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The crop/format code the API suggests for this asset (`bb` = letterbox
    /// fit with background fill, `sr` = source-ratio fill, ...). Acts as the
    /// "jam"/shared-listening fit hint when present.
    #[serde(rename = "defaultCropCode")]
    pub default_crop_code: Option<String>,
    #[serde(rename = "bgColor")]
    pub bg_color: Option<String>,
    #[serde(rename = "textColor1")]
    pub text_color1: Option<String>,
    #[serde(rename = "textColor2")]
    pub text_color2: Option<String>,
    #[serde(rename = "textColor3")]
    pub text_color3: Option<String>,
    #[serde(rename = "textColor4")]
    pub text_color4: Option<String>,
    #[serde(rename = "hasP3")]
    pub has_p3: bool,
}

impl Artwork {
    /// Render the template at `w`×`h` (e.g. `.../{w}x{h}bb.jpg` →
    /// `.../600x600bb.jpg`). Placeholder and pre-rendered URLs both work. A
    /// template with a literal crop code keeps it; `{c}` uses the API's
    /// `defaultCropCode` (else `bb`).
    #[allow(dead_code)] // public render API for hit payloads; app renders via Track::artwork_url
    pub fn url(&self, w: u32, h: u32) -> String {
        render_artwork(&self.url, w, h, self.default_crop_code.as_deref(), None)
    }

    /// Square letterbox "fit" render at `size`×`size` (jam/shared-listening
    /// style: image fitted inside the box, background filled).
    #[allow(dead_code)] // public render API for hit payloads; see `Artwork::url`
    pub fn fit_url(&self, size: u32) -> String {
        render_artwork(&self.url, size, size, Some("bb"), None)
    }
}

impl Track {
    /// Re-render this track's artwork at a different size. `Track::artwork`
    /// holds a concrete 600×600 URL; its size token is swapped in place.
    pub fn artwork_url(&self, w: u32, h: u32) -> Option<String> {
        self.artwork
            .as_deref()
            .map(|u| render_artwork(u, w, h, None, None))
    }
}

/// A user library playlist (`/v1/me/library/playlists`).
#[derive(Clone, Debug, Default)]
pub struct LibraryPlaylist {
    pub id: String,
    pub name: String,
    /// `description.standard` when present, else empty.
    pub description: String,
    pub track_count: u32,
    pub artwork: Option<Artwork>,
}

/// Catalog artist with top songs (`/v1/catalog/{sf}/artists/{id}?views=top-songs`).
#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // catalog-artist payload returned by `Api::artist`; no reader yet
pub struct Artist {
    pub id: String,
    pub name: String,
    pub genres: String,
    pub artwork: Option<Artwork>,
    pub top_songs: Vec<Track>,
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
    #[allow(dead_code)] // wire field kept for payload completeness (previews unused)
    #[serde(default)]
    previews: Vec<RawPreview>,
    #[serde(default)]
    play_params: Option<RawPlayParams>,
    track_count: u32,
    release_date: String,
    /// Library resources only (`dateAdded`).
    date_added: Option<String>,
    genre_names: Vec<String>,
    /// Library playlists only: `{ "standard": "..." }`.
    description: Option<RawDescription>,
}

#[derive(Deserialize, Default)]
struct RawDescription {
    #[serde(default)]
    standard: String,
}

#[derive(Deserialize, Default)]
struct RawPlayParams {
    #[serde(default, rename = "catalogId")]
    catalog_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawArtwork {
    url: String,
    width: Option<u32>,
    height: Option<u32>,
    #[serde(rename = "defaultCropCode")]
    default_crop_code: Option<String>,
    #[serde(rename = "bgColor")]
    bg_color: Option<String>,
    #[serde(rename = "textColor1")]
    text_color1: Option<String>,
    #[serde(rename = "textColor2")]
    text_color2: Option<String>,
    #[serde(rename = "textColor3")]
    text_color3: Option<String>,
    #[serde(rename = "textColor4")]
    text_color4: Option<String>,
    #[serde(rename = "hasP3")]
    has_p3: bool,
}

impl RawArtwork {
    fn to_artwork(&self) -> Artwork {
        Artwork {
            url: self.url.clone(),
            width: self.width,
            height: self.height,
            default_crop_code: self.default_crop_code.clone(),
            bg_color: self.bg_color.clone(),
            text_color1: self.text_color1.clone(),
            text_color2: self.text_color2.clone(),
            text_color3: self.text_color3.clone(),
            text_color4: self.text_color4.clone(),
            has_p3: self.has_p3,
        }
    }
}

#[derive(Deserialize, Default)]
struct RawPreview {
    #[allow(dead_code)] // wire field kept for payload completeness
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

    /// Raw artwork object (template + palette) for size-aware rendering.
    fn artwork_obj(&self) -> Option<Artwork> {
        self.attributes.artwork.as_ref().map(|a| a.to_artwork())
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
            date_added: self.attributes.date_added.clone(),
        }
    }

    fn to_artist(&self) -> ArtistHit {
        ArtistHit {
            name: self.attributes.name.clone(),
            genres: self.attributes.genre_names.join(", "),
            artwork_obj: self.artwork_obj(),
        }
    }

    /// A library playlist resource.
    fn to_playlist(&self) -> LibraryPlaylist {
        LibraryPlaylist {
            id: self.id.clone(),
            name: self.attributes.name.clone(),
            description: self
                .attributes
                .description
                .as_ref()
                .map(|d| d.standard.clone())
                .unwrap_or_default(),
            track_count: self.attributes.track_count,
            artwork: self.artwork_obj(),
        }
    }
}

#[derive(Deserialize, Default)]
struct RawGroup {
    #[serde(default)]
    data: Vec<RawResource>,
    #[serde(default)]
    next: Option<String>,
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

/// `/v1/catalog/{sf}/artists/{id}?views=top-songs` payload.
#[derive(Deserialize, Default)]
struct RawArtistResp {
    #[serde(default)]
    data: Vec<RawArtist>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawArtist {
    id: String,
    attributes: RawAttrs,
    views: RawArtistViews,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawArtistViews {
    #[serde(rename = "top-songs")]
    top_songs: RawGroup,
}

impl RawArtist {
    fn artwork_obj(&self) -> Option<Artwork> {
        self.attributes.artwork.as_ref().map(|a| a.to_artwork())
    }
}

/// True when `id` is a numeric catalog adam id (as opposed to a library id
/// like `l.pxdMFlY` or `i.0abc`, which the catalog endpoints reject with 404).
fn is_catalog_album_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())
}

/// URL for one page of the user's library albums, with an optional
/// server-side `sort` value (e.g. `-dateAdded`).
fn library_albums_url(limit: u32, offset: u32, sort: Option<&str>) -> String {
    match sort {
        Some(s) => format!(
            "https://amp-api.music.apple.com/v1/me/library/albums?limit={limit}&offset={offset}&sort={s}&l=en-US"
        ),
        None => format!(
            "https://amp-api.music.apple.com/v1/me/library/albums?limit={limit}&offset={offset}&l=en-US"
        ),
    }
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
    /// Pending user-facing notice from the cache layer (e.g. "served a
    /// stale copy because the network is down"); consumed by the UI/CLI
    /// after a fetch via [`Api::take_notice`].
    notice: RefCell<Option<String>>,
}

impl Api {
    pub fn new(cfg: Config) -> Result<Self> {
        Self::with_timeouts(cfg, Duration::from_secs(5), Duration::from_secs(20))
    }

    /// Api with custom timeouts; the play reporter uses a shorter budget so
    /// app exit never blocks long on the final report POST.
    pub fn with_timeouts(cfg: Config, connect: Duration, total: Duration) -> Result<Self> {
        cfg.ensure_cache_dir()?;
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(connect)
            .timeout(total)
            .user_agent(UA)
            .build();
        Ok(Self {
            agent,
            cfg,
            bearer: RefCell::new(None),
            notice: RefCell::new(None),
        })
    }

    /// Bearer JWT scraped from the web player, cached on disk ~12h.
    pub fn bearer(&self) -> Result<String> {
        if let Some(t) = self.bearer.borrow().as_ref() {
            return Ok(t.clone());
        }
        let cache = cache_path(&self.cfg, "bearer.jwt")?;
        if fresh(&cache, 12 * 3600)
            && let Ok(t) = std::fs::read_to_string(&cache)
        {
            let t = t.trim().to_string();
            if !t.is_empty() {
                *self.bearer.borrow_mut() = Some(t.clone());
                return Ok(t);
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
        if let Some(t) = &self.cfg.media_user_token
            && !t.is_empty()
        {
            req = req.set("Media-User-Token", t);
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        resp.into_json().with_context(|| format!("body of {url}"))
    }

    /// GET with a single transparent 401-retry after bearer refresh, fronted
    /// by the metadata disk cache: fresh TTL → serve cached; network failure
    /// with any cached copy → serve stale (offline browsing), flagged via
    /// [`Api::take_notice`] so the UI can say so. Auth failures (401/403)
    /// are never masked by stale data — they surface with a hint to fix the
    /// token instead.
    fn get_json_retry(&self, url: &str) -> Result<serde_json::Value> {
        if let Some(v) = self.meta_lookup(url) {
            return Ok(v);
        }
        let mut err = match self.get_json(url) {
            Ok(v) => {
                self.meta_store(url, &v);
                return Ok(v);
            }
            Err(e) => e,
        };
        if is_auth_error(&err) {
            match self.refresh_bearer().and_then(|_| self.get_json(url)) {
                Ok(v) => {
                    self.meta_store(url, &v);
                    return Ok(v);
                }
                Err(e) => err = e,
            }
            if is_auth_error(&err) {
                // Still unauthorized on a freshly scraped bearer: the
                // media-user-token (or account) is the problem, and stale
                // data must not hide that.
                return Err(self.auth_error(err));
            }
            // the refresh itself could not reach the network → fall through
            // to the offline fallback below
        }
        // network failed (offline / down) — any cached copy beats an error
        // for browsing, but say so
        if let Some((v, stored)) = self.meta_stale(url) {
            let kind = if url.contains("/v1/me/") { "library" } else { "catalog" };
            let age = human_age(unix_now().saturating_sub(stored));
            self.notice
                .borrow_mut()
                .replace(format!("showing cached {kind} from {age} — offline?"));
            return Ok(v);
        }
        Err(err)
    }

    /// User-facing message for an auth failure that survived a bearer
    /// refresh. With a media-user-token configured (library, playlists and
    /// library search all depend on it), the token is the prime suspect.
    fn auth_error(&self, e: anyhow::Error) -> anyhow::Error {
        if self
            .cfg
            .media_user_token
            .as_deref()
            .is_some_and(|t| !t.is_empty())
        {
            anyhow::anyhow!("media-user-token expired — update it in the config")
        } else {
            anyhow::anyhow!("Apple Music authentication failed — {e:#}")
        }
    }

    /// Consume a pending notice from the cache layer, if the last fetch set
    /// one (currently: a stale copy served past its TTL on network failure).
    pub fn take_notice(&self) -> Option<String> {
        self.notice.borrow_mut().take()
    }

    // ---- metadata disk cache ({cache_dir}/meta) ----
    //
    // Key = FNV-1a-128 of (URL + storefront); the file is the raw response
    // JSON prefixed with a one-line JSON header `{"stored":…,"ttl":…}`.
    // Raw JSON is cached (serde structs untouched). Failures are silent and
    // never fail the request.

    fn meta_path(&self, url: &str) -> Option<PathBuf> {
        let key = fnv128(format!("{url}\x1f{}", self.cfg.storefront).as_bytes());
        cache_path(&self.cfg, &format!("meta/{key}.cache")).ok()
    }

    /// Fresh TTL hit. A missing/garbage header counts as a miss (treated
    /// like an expired entry: refetch and overwrite), never as a hit.
    fn meta_lookup(&self, url: &str) -> Option<serde_json::Value> {
        let raw = std::fs::read_to_string(self.meta_path(url)?).ok()?;
        let (head, body) = raw.split_once('\n')?;
        let head: serde_json::Value = serde_json::from_str(head).ok()?;
        let stored = head.get("stored")?.as_u64()?;
        let ttl = head.get("ttl")?.as_u64()?;
        if !meta_fresh(stored, unix_now(), ttl) {
            return None;
        }
        serde_json::from_str(body).ok()
    }

    /// Any cached copy, regardless of age (network-failure fallback).
    /// Returns the value plus a `stored` epoch-seconds stamp — the header's
    /// when parseable, otherwise the file's mtime — so the stale-serve
    /// notice can state the copy's age.
    fn meta_stale(&self, url: &str) -> Option<(serde_json::Value, u64)> {
        let p = self.meta_path(url)?;
        let raw = std::fs::read_to_string(&p).ok()?;
        let (head, body) = raw.split_once('\n')?;
        let stored = serde_json::from_str::<serde_json::Value>(head)
            .ok()
            .and_then(|h| h.get("stored").and_then(|s| s.as_u64()))
            .or_else(|| {
                let m = std::fs::metadata(&p).ok()?;
                let t = m.modified().ok()?;
                t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
            })?;
        serde_json::from_str(body).ok().map(|v| (v, stored))
    }

    fn meta_store(&self, url: &str, v: &serde_json::Value) {
        let Some(p) = self.meta_path(url) else { return };
        let body = match serde_json::to_vec(v) {
            Ok(b) => b,
            Err(_) => return,
        };
        let mut raw = format!(
            "{{\"stored\":{},\"ttl\":{}}}\n",
            unix_now(),
            meta_ttl(url)
        )
        .into_bytes();
        raw.extend_from_slice(&body);
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, &raw).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
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
        self.library_albums_page(limit, offset, None).map(|(albums, _)| albums)
    }

    /// One page of the user's library albums. Returns the albums plus the
    /// offset for the next page (None = end), mirroring `library_songs`.
    /// `sort` is a server-side `sort` value; empirically (2026-10) Apple
    /// accepts only `dateAdded` (with `-` for descending) on this endpoint —
    /// name sorts are rejected with 400. None = server default, which is
    /// alphabetical by title (new additions then fall outside page 0).
    pub fn library_albums_page(
        &self,
        limit: u32,
        offset: u32,
        sort: Option<&str>,
    ) -> Result<(Vec<AlbumHit>, Option<u32>)> {
        let url = library_albums_url(limit, offset, sort);
        let raw: RawLibraryResp = from_value(self.get_json_retry(&url)?)?;
        let albums = raw.data.iter().map(|r| r.to_album()).collect();
        Ok((albums, raw.next.as_deref().and_then(parse_next_offset)))
    }

    /// User library playlists (requires media-user-token). `extend` asks the
    /// API for `trackCount`/`description`, which the plain list omits. Returns
    /// playlists plus the offset for the next page (None = end).
    pub fn library_playlists(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<LibraryPlaylist>, Option<u32>)> {
        let url = format!(
            "https://amp-api.music.apple.com/v1/me/library/playlists?limit={limit}&offset={offset}&extend=trackCount%2Cdescription&l=en-US"
        );
        let raw: RawLibraryResp = from_value(self.get_json_retry(&url)?)?;
        let lists = raw.data.iter().map(|r| r.to_playlist()).collect();
        Ok((lists, raw.next.as_deref().and_then(parse_next_offset)))
    }

    /// Tracks of any album: numeric adam id → catalog endpoint, anything
    /// else (library ids: songs `i.`, albums `l.`) → the library-tracks
    /// endpoint (catalog endpoints 404 on library ids).
    pub fn album_tracks_any(&self, id: &str) -> Result<Vec<Track>> {
        if is_catalog_album_id(id) {
            self.album_tracks(id)
        } else {
            let url = format!(
                "https://amp-api.music.apple.com/v1/me/library/albums/{id}/tracks?limit=300&l=en-US"
            );
            // A 404 here means the album was removed from the library after
            // the listing was fetched — say that instead of surfacing a raw
            // HTTP error.
            let body = self.get_json_retry(&url).map_err(|e| {
                if e.to_string().contains("404") {
                    anyhow::anyhow!(
                        "this album is no longer in your library (it may have been removed) — refresh the Library view"
                    )
                } else {
                    e
                }
            })?;
            let raw: RawGroup = from_value(body)?;
            Ok(raw.data.iter().map(|r| r.to_track()).collect())
        }
    }

    /// One page of a library playlist's tracks. Returns the tracks plus the
    /// offset for the next page (None = end), mirroring `library_songs`.
    pub fn playlist_tracks_page(
        &self,
        id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<Track>, Option<u32>)> {
        let url = format!(
            "https://amp-api.music.apple.com/v1/me/library/playlists/{id}/tracks?limit={limit}&offset={offset}&l=en-US"
        );
        let raw: RawGroup = from_value(self.get_json_retry(&url)?)?;
        let tracks = raw.data.iter().map(|r| r.to_track()).collect();
        Ok((tracks, raw.next.as_deref().and_then(parse_next_offset)))
    }

    /// Fetch a single catalog song by adamId.
    pub fn search_by_id(&self, id: &str) -> Result<Option<Track>> {
        let url = format!(
            "https://amp-api.music.apple.com/v1/catalog/{}/songs/{id}?l=en-US",
            self.cfg.storefront
        );
        let raw: RawGroup = from_value(self.get_json_retry(&url)?)?;
        Ok(raw.data.first().map(|r| r.to_track()))
    }

    /// Catalog artist with top songs. Fetches the `top-songs` view and follows
    /// its pagination (capped at 100 songs).
    #[allow(dead_code)] // catalog-artist API kept complete; no artist view yet (fixture-tested)
    pub fn artist(&self, id: &str) -> Result<Artist> {
        let url = format!(
            "https://amp-api.music.apple.com/v1/catalog/{}/artists/{id}?views=top-songs&l=en-US",
            self.cfg.storefront
        );
        let raw: RawArtistResp =
            from_value(self.get_json_retry(&url)?).context("decoding artist response")?;
        let artist = raw.data.into_iter().next().context("artist not found")?;

        let mut top_songs: Vec<Track> = artist
            .views
            .top_songs
            .data
            .iter()
            .map(|r| r.to_track())
            .collect();
        let mut next = artist.views.top_songs.next.clone();
        while let Some(n) = next {
            if top_songs.len() >= 100 {
                break;
            }
            let page: RawGroup = from_value(self.get_json_retry(&api_url(&n))?)?;
            if page.data.is_empty() {
                break;
            }
            top_songs.extend(page.data.iter().map(|r| r.to_track()));
            next = page.next.clone();
        }

        let artwork = artist.artwork_obj();
        Ok(Artist {
            id: artist.id,
            name: artist.attributes.name.clone(),
            genres: artist.attributes.genre_names.join(", "),
            artwork,
            top_songs,
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

    /// POST play-activity events to Apple's Universal Activity Service
    /// (`JSPLAY` channel). The web player sends ONE event per POST; do the
    /// same (pass a one-element slice per call). Best-effort by contract —
    /// the caller swallows failures, there is no retry.
    pub fn report_activity(&self, events: &[PlayEvent]) -> Result<()> {
        const URL: &str = "https://universal-activity-service.itunes.apple.com/play";
        let bearer = self.bearer()?;
        let mut req = self
            .agent
            .post(URL)
            .set("content-type", "application/json")
            .set("Authorization", &format!("Bearer {bearer}"))
            .set("origin", "https://music.apple.com")
            .set("referer", "https://music.apple.com/");
        if let Some(t) = &self.cfg.media_user_token
            && !t.is_empty()
        {
            req = req.set("media-user-token", t);
        }
        req.send_json(JsPlayBody {
            client_id: "JSCLIENT",
            event_type: "JSPLAY",
            data: events,
        })
        .with_context(|| format!("POST {URL}"))?
        .into_string()
        .with_context(|| format!("body of {URL}"))?;
        Ok(())
    }
}

// ---- play reporting (universal-activity-service JSPLAY wire shapes) ----

/// Body `build-version` mirroring the Windows web player; not validated
/// server-side but kept byte-identical to the observed payload.
pub const REPORT_BUILD_VERSION: &str =
    "AppleMusic/1.0 Windows/nt 10.0; win64; x64 model/Win32 build/2626.2.0-external";
/// Body `user-agent` (a *body field* of every report, not the HTTP header).
pub const REPORT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

/// `play-mode` triple. Values as sent by music.apple.com: repeat
/// 1 = off / 2 = all / 3 = one, shuffle 1 = off / 2 = on, auto
/// 1 = no autoplay-after-queue / 2 = autoplay enabled.
#[derive(Clone, Debug, Serialize)]
pub struct PlayMode {
    #[serde(rename = "auto-play-mode")]
    pub auto: u8,
    #[serde(rename = "repeat-play-mode")]
    pub repeat: u8,
    #[serde(rename = "shuffle-play-mode")]
    pub shuffle: u8,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlayIds {
    /// Numeric catalog adam id as a STRING (never a `i.`/`l.` library id).
    #[serde(rename = "subscription-adam-id")]
    pub subscription_adam_id: String,
    #[serde(rename = "cloud-id", skip_serializing_if = "Option::is_none")]
    pub cloud_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContainerIds {
    #[serde(rename = "album-adam-id")]
    pub album_adam_id: String,
}

/// One JSPLAY `data[]` element. Field names are Apple's hyphenated wire
/// names; START-only / END-only / optional fields are Options so exactly the
/// expected key set serializes.
#[derive(Clone, Debug, Serialize)]
pub struct PlayEvent {
    #[serde(rename = "build-version")]
    pub build_version: String,
    #[serde(rename = "developer-token")]
    pub developer_token: String,
    /// 1 = PLAY_START, 0 = PLAY_END.
    #[serde(rename = "event-type")]
    pub event_type: u8,
    pub ids: PlayIds,
    #[serde(rename = "internal-build")]
    pub internal_build: bool,
    #[serde(rename = "media-duration-in-milliseconds")]
    pub media_duration_ms: u64,
    /// 0 = audio, 1 = video.
    #[serde(rename = "media-type")]
    pub media_type: u8,
    #[serde(rename = "milliseconds-since-play")]
    pub milliseconds_since_play: u64,
    pub offline: bool,
    /// Fresh random 16-hex per event (rotated by the reporter at send time).
    #[serde(rename = "persistent-id")]
    pub persistent_id: String,
    #[serde(rename = "play-mode")]
    pub play_mode: PlayMode,
    #[serde(rename = "private-enabled")]
    pub private_enabled: bool,
    #[serde(rename = "sb-enabled")]
    pub sb_enabled: bool,
    #[serde(rename = "siri-initiated")]
    pub siri_initiated: bool,
    /// 16 = WEB_APP.
    #[serde(rename = "source-type")]
    pub source_type: u8,
    #[serde(rename = "start-position-in-milliseconds")]
    pub start_position_ms: u64,
    /// Lowercased storefront.
    #[serde(rename = "store-front")]
    pub store_front: String,
    /// 1 = ITUNES_STORE_CONTENT, 4 = STREAM (live radio).
    #[serde(rename = "type")]
    pub item_type: u8,
    #[serde(rename = "user-agent")]
    pub user_agent: String,
    #[serde(rename = "user-token")]
    pub user_token: String,
    /// Seconds EAST of UTC (US timezones send negative values).
    #[serde(rename = "utc-offset-in-seconds")]
    pub utc_offset_seconds: i32,
    /// START only: 0 = not specified, 1 = container changed.
    #[serde(
        rename = "event-reason-hint-type",
        skip_serializing_if = "Option::is_none"
    )]
    pub event_reason_hint_type: Option<u8>,
    /// END only.
    #[serde(
        rename = "end-position-in-milliseconds",
        skip_serializing_if = "Option::is_none"
    )]
    pub end_position_ms: Option<u64>,
    /// END only: 2 skip-fwd, 3 pause, 4 suspend/stop, 5 replaced, 7 natural,
    /// 12 scrub-begin, 13 scrub-end, 14 skip-back.
    #[serde(rename = "end-reason-type", skip_serializing_if = "Option::is_none")]
    pub end_reason_type: Option<u8>,
    /// Always serialized (0 = none); ids only when the container is known.
    #[serde(rename = "container-type")]
    pub container_type: u8,
    #[serde(rename = "container-ids", skip_serializing_if = "Option::is_none")]
    pub container_ids: Option<ContainerIds>,
}

impl PlayEvent {
    pub const EVENT_TYPE_PLAY_END: u8 = 0;
    pub const EVENT_TYPE_PLAY_START: u8 = 1;
    // end-reason-type
    pub const END_SKIPPED_FORWARDS: u8 = 2;
    pub const END_PAUSED: u8 = 3;
    pub const END_SUSPENDED: u8 = 4;
    pub const END_REPLACED: u8 = 5;
    pub const END_NATURAL: u8 = 7;
    pub const END_SCRUB_BEGIN: u8 = 12;
    pub const END_SCRUB_END: u8 = 13;
    pub const END_SKIPPED_BACKWARDS: u8 = 14;
    // event-reason-hint-type
    pub const HINT_NONE: u8 = 0;
    pub const HINT_CONTAINER_CHANGED: u8 = 1;
    // container-type
    pub const CONTAINER_NONE: u8 = 0;
    pub const CONTAINER_ALBUM: u8 = 3;

    fn base(adam_id: &str, duration_ms: u64, mode: PlayMode, album_adam_id: Option<String>) -> Self {
        Self {
            build_version: REPORT_BUILD_VERSION.into(),
            developer_token: String::new(),
            event_type: 0,
            ids: PlayIds {
                subscription_adam_id: adam_id.to_string(),
                cloud_id: None,
            },
            internal_build: false,
            media_duration_ms: duration_ms,
            media_type: 0,
            milliseconds_since_play: 1,
            offline: false,
            persistent_id: String::new(),
            play_mode: mode,
            private_enabled: false,
            sb_enabled: true,
            siri_initiated: false,
            source_type: 16,
            start_position_ms: 0,
            store_front: String::new(),
            item_type: 1,
            user_agent: REPORT_USER_AGENT.into(),
            user_token: String::new(),
            utc_offset_seconds: 0,
            event_reason_hint_type: None,
            end_position_ms: None,
            end_reason_type: None,
            container_type: if album_adam_id.is_some() {
                Self::CONTAINER_ALBUM
            } else {
                Self::CONTAINER_NONE
            },
            container_ids: album_adam_id.map(|id| ContainerIds {
                album_adam_id: id,
            }),
        }
    }

    /// A PLAY_START. `hint`: 1 when the container changed vs the previously
    /// reported one, else 0.
    pub fn start(
        adam_id: &str,
        duration_ms: u64,
        start_position_ms: u64,
        hint: u8,
        mode: PlayMode,
        album_adam_id: Option<String>,
    ) -> Self {
        let mut ev = Self::base(adam_id, duration_ms, mode, album_adam_id);
        ev.event_type = Self::EVENT_TYPE_PLAY_START;
        ev.start_position_ms = start_position_ms;
        ev.event_reason_hint_type = Some(hint);
        ev
    }

    /// A PLAY_END. `start_position_ms` chains from the open segment's start;
    /// natural ends pass the full duration as `end_position_ms`.
    #[allow(clippy::too_many_arguments)]
    pub fn end(
        adam_id: &str,
        duration_ms: u64,
        start_position_ms: u64,
        end_position_ms: u64,
        reason: u8,
        mode: PlayMode,
        album_adam_id: Option<String>,
    ) -> Self {
        let mut ev = Self::base(adam_id, duration_ms, mode, album_adam_id);
        ev.event_type = Self::EVENT_TYPE_PLAY_END;
        ev.start_position_ms = start_position_ms;
        ev.end_position_ms = Some(end_position_ms);
        ev.end_reason_type = Some(reason);
        ev
    }
}

/// `{client_id:"JSCLIENT",event_type:"JSPLAY",data:[…]}` envelope.
#[derive(Serialize)]
struct JsPlayBody<'a> {
    client_id: &'static str,
    event_type: &'static str,
    data: &'a [PlayEvent],
}

// ---- helpers ----

/// Metadata-cache freshness windows: catalog data barely changes, library
/// data changes with every add/play.
const META_TTL_CATALOG: u64 = 7 * 24 * 3600;

/// Library TTL; the Library view also uses it to decide when its in-memory
/// page is too old to keep showing without a refetch.
pub const META_TTL_LIBRARY: u64 = 3600;

/// TTL for an amp-api URL: library endpoints (`/v1/me/`) 1h, catalog 7d.
fn meta_ttl(url: &str) -> u64 {
    if url.contains("/v1/me/") {
        META_TTL_LIBRARY
    } else {
        META_TTL_CATALOG
    }
}

/// Pure freshness decision (clock injected as `now` for tests).
///
/// A `stored` stamp in the future is garbage — clock skew, or a foreign /
/// pre-project writer of the same cache layout — and must NOT count as
/// maximally fresh: with saturating arithmetic it would be served forever,
/// the sweep would keep it forever, and the network would never be
/// consulted again (a Library frozen for weeks with no way to self-heal).
/// Refusing it makes the entry stale → refetch → overwrite with a sane
/// stamp.
fn meta_fresh(stored: u64, now: u64, ttl: u64) -> bool {
    stored <= now && now - stored < ttl
}

/// "45 minutes" / "3 hours" / "2 days" for the stale-serve notice.
fn human_age(age_secs: u64) -> String {
    let (n, unit) = if age_secs < 3600 {
        (age_secs / 60, "minute")
    } else if age_secs < 24 * 3600 {
        (age_secs / 3600, "hour")
    } else {
        (age_secs / (24 * 3600), "day")
    };
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// Startup sweep: delete metadata cache entries older than their TTL
/// (best-effort; the audio cache is budget-managed separately).
pub fn clean_meta_cache(cfg: &Config) {
    let dir = cfg.cache_dir.join("meta");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = unix_now();
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() || !e.file_name().to_string_lossy().ends_with(".cache") {
            continue;
        }
        let expired = std::fs::read_to_string(e.path())
            .ok()
            .and_then(|raw| raw.split_once('\n').map(|(h, _)| h.to_string()))
            .and_then(|h| serde_json::from_str::<serde_json::Value>(&h).ok())
            .map(|v| {
                let stored = v.get("stored").and_then(|s| s.as_u64()).unwrap_or(0);
                let ttl = v.get("ttl").and_then(|t| t.as_u64()).unwrap_or(0);
                !meta_fresh(stored, now, ttl)
            })
            .unwrap_or(true); // unparsable junk → drop
        if expired {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn from_value<T: DeserializeOwned>(v: serde_json::Value) -> Result<T> {
    serde_json::from_value(v).map_err(Into::into)
}

/// True when the error is an HTTP auth failure (401/403). These are never
/// masked by the stale-cache fallback: serving a weeks-old library on top of
/// a dead token hides the actual problem from the user.
fn is_auth_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<ureq::Error>()
            .is_some_and(|ue| matches!(ue, ureq::Error::Status(401 | 403, _)))
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

/// Absolute amp-api URL for a `next`/`href` value that may be a bare path.
fn api_url(next: &str) -> String {
    if next.starts_with("http://") || next.starts_with("https://") {
        next.to_string()
    } else {
        format!("https://amp-api.music.apple.com{next}")
    }
}

/// Render an Apple artwork URL template.
///
/// Handles both `{w}`/`{h}`/`{c}`/`{f}` placeholders (and their percent-encoded
/// forms) and already-rendered URLs (whose size token is swapped). `crop`/
/// `fmt` override the template's own literal values when supplied.
fn render_artwork(tpl: &str, w: u32, h: u32, crop: Option<&str>, fmt: Option<&str>) -> String {
    if tpl.is_empty() {
        return String::new();
    }
    let mut out = tpl.to_string();

    let had_wh = out.contains("{w}") || out.contains("%7Bw%7D");
    out = out
        .replace("{w}", &w.to_string())
        .replace("%7Bw%7D", &w.to_string())
        .replace("{h}", &h.to_string())
        .replace("%7Bh%7D", &h.to_string());
    if !had_wh {
        // Already-rendered URL: swap the concrete `NNNxNNN` size token.
        out = replace_size_token(&out, w, h);
    }

    let fallback_crop = crop.unwrap_or("bb");
    if out.contains("{c}") || out.contains("%7Bc%7D") {
        out = out
            .replace("{c}", fallback_crop)
            .replace("%7Bc%7D", fallback_crop);
    } else if let Some(c) = crop {
        out = replace_crop_code(&out, c);
    }

    if out.contains("{f}") || out.contains("%7Bf%7D") {
        let fallback_fmt = fmt.unwrap_or("jpg");
        out = out
            .replace("{f}", fallback_fmt)
            .replace("%7Bf%7D", fallback_fmt);
    }

    out
}

/// Replace the first `NNNxNNN` dimension token with `w`x`h`.
fn replace_size_token(s: &str, w: u32, h: u32) -> String {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_digit()) {
            let start = i;
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j < b.len() && (b[j] == b'x' || b[j] == b'X') {
                let mut e = j + 1;
                while e < b.len() && b[e].is_ascii_digit() {
                    e += 1;
                }
                if e > j + 1 {
                    let mut out = String::with_capacity(s.len() + 8);
                    out.push_str(&s[..start]);
                    out.push_str(&format!("{w}x{h}"));
                    out.push_str(&s[e..]);
                    return out;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    s.to_string()
}

/// Swap the two-letter crop code that precedes the final `.ext` in an artwork
/// URL (e.g. `.../600x600sr.jpg` → `.../600x600bb.jpg`).
fn replace_crop_code(s: &str, crop: &str) -> String {
    if let Some(dot) = s.rfind('.') {
        let b = s.as_bytes();
        if dot >= 2 && b[dot - 1].is_ascii_lowercase() && b[dot - 2].is_ascii_lowercase() {
            let mut out = String::with_capacity(s.len());
            out.push_str(&s[..dot - 2]);
            out.push_str(crop);
            out.push_str(&s[dot..]);
            return out;
        }
    }
    s.to_string()
}


/// Extract a catalog id from an Apple Music URL or return the input as-is.
pub fn id_from_arg(arg: &str) -> Result<String> {
    if let Some(id) = arg.split('?').next().unwrap_or(arg).rsplit('/').next()
        && id.chars().all(|c| c.is_ascii_digit())
        && !id.is_empty()
    {
        return Ok(id.to_string());
    }
    if arg.chars().all(|c| c.is_ascii_digit()) {
        return Ok(arg.to_string());
    }
    anyhow::bail!("cannot parse id from {arg:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_ttl_splits_catalog_from_library() {
        assert_eq!(meta_ttl("https://amp-api.music.apple.com/v1/catalog/us/search?term=x"), META_TTL_CATALOG);
        assert_eq!(meta_ttl("https://amp-api.music.apple.com/v1/catalog/us/artists/5"), META_TTL_CATALOG);
        assert_eq!(meta_ttl("https://amp-api.music.apple.com/v1/me/library/songs?limit=100"), META_TTL_LIBRARY);
        assert_eq!(meta_ttl("https://amp-api.music.apple.com/v1/me/library/search?term=x"), META_TTL_LIBRARY);
    }

    #[test]
    fn meta_fresh_ttl_decision() {
        let ttl = 3600;
        assert!(meta_fresh(1_000, 1_000, ttl)); // just stored
        assert!(meta_fresh(1_000, 1_000 + ttl - 1, ttl)); // inside window
        assert!(!meta_fresh(1_000, 1_000 + ttl, ttl)); // boundary: stale
        assert!(!meta_fresh(1_000, 2_000 + ttl, ttl));
        // a stamp in the future is garbage (clock skew / foreign writer):
        // it must count as stale so the entry is refetched and overwritten
        // instead of being served — and kept by the sweep — forever
        assert!(!meta_fresh(2_000, 1_000, ttl));
        // a milliseconds-unit stamp (1.79e12) is just a huge future stamp:
        // same refusal
        assert!(!meta_fresh(1_791_502_234_000, 1_791_502_234, ttl));
        assert_eq!(meta_ttl(""), META_TTL_CATALOG); // default is catalog
    }

    #[test]
    fn human_age_formats_minutes_hours_days() {
        assert_eq!(human_age(0), "0 minutes");
        assert_eq!(human_age(59), "0 minutes");
        assert_eq!(human_age(60), "1 minute");
        assert_eq!(human_age(45 * 60), "45 minutes");
        assert_eq!(human_age(3600), "1 hour");
        assert_eq!(human_age(23 * 3600), "23 hours");
        assert_eq!(human_age(24 * 3600), "1 day");
        assert_eq!(human_age(3 * 24 * 3600 + 500), "3 days");
    }

    #[test]
    fn auth_errors_detected_401_403_only() {
        fn status_err(code: u16) -> anyhow::Error {
            let resp = ureq::Response::new(code, "test", "{}").unwrap();
            anyhow::Error::new(ureq::Error::Status(code, resp))
        }
        assert!(is_auth_error(&status_err(401)));
        assert!(is_auth_error(&status_err(403)));
        assert!(!is_auth_error(&status_err(404)));
        assert!(!is_auth_error(&status_err(500)));
        assert!(!is_auth_error(&anyhow::Error::msg("connection refused")));
        // context-wrapped errors still detected through the chain
        assert!(is_auth_error(&status_err(401).context("GET https://x")));
    }

    #[test]
    fn meta_hash_is_stable_hex_and_input_sensitive() {
        let a = fnv128(b"https://amp-api.music.apple.com/v1/catalog/us/search?term=daft+punk\x1fus");
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, fnv128(b"https://amp-api.music.apple.com/v1/catalog/us/search?term=daft+punk\x1fus"));
        assert_ne!(a, fnv128(b"https://amp-api.music.apple.com/v1/catalog/us/search?term=daft+punk\x1fgb"));
        assert_ne!(a, fnv128(b"https://amp-api.music.apple.com/v1/catalog/us/search?term=daft+punkz\x1fus"));
    }

    #[test]
    fn meta_roundtrip_through_disk() {
        let dir = std::env::temp_dir().join(format!("mordent-meta-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            cache_dir: dir.clone(),
            ..Config::default()
        };
        let api = Api::new(cfg).unwrap();
        let url = "https://amp-api.music.apple.com/v1/catalog/us/search?term=roundtrip";
        let v = serde_json::json!({ "results": { "songs": { "data": [1, 2, 3] } } });

        assert!(api.meta_lookup(url).is_none());
        api.meta_store(url, &v);
        // fresh → identical value from disk
        assert_eq!(api.meta_lookup(url).as_ref(), Some(&v));
        // stale → lookup declines but the stale fallback still serves it
        let p = api.meta_path(url).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        let (_, body) = raw.split_once('\n').unwrap();
        let old = format!("{{\"stored\":0,\"ttl\":{}}}\n{body}", META_TTL_CATALOG);
        std::fs::write(&p, old).unwrap();
        assert!(api.meta_lookup(url).is_none());
        assert_eq!(api.meta_stale(url).map(|(v, _)| v).as_ref(), Some(&v));
        // store overwrites (header refresh)
        api.meta_store(url, &v);
        assert_eq!(api.meta_lookup(url).as_ref(), Some(&v));

        // startup sweep drops expired entries, keeps fresh ones
        let fresh_url = "https://amp-api.music.apple.com/v1/catalog/us/search?term=fresh";
        api.meta_store(fresh_url, &v);
        let fp = api.meta_path(fresh_url).unwrap();
        // expire `url` again after the overwrite (stored=0 < any ttl)
        let raw = std::fs::read_to_string(&p).unwrap();
        let (_, body) = raw.split_once('\n').unwrap();
        std::fs::write(&p, format!("{{\"stored\":0,\"ttl\":{}}}\n{body}", META_TTL_CATALOG)).unwrap();
        let cfg = Config {
            cache_dir: dir.clone(),
            ..Config::default()
        };
        clean_meta_cache(&cfg);
        assert!(!p.exists(), "expired entry swept");
        assert!(fp.exists(), "fresh entry kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn meta_future_and_malformed_headers_never_served_fresh() {
        let dir = std::env::temp_dir().join(format!("mordent-meta-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            cache_dir: dir.clone(),
            ..Config::default()
        };
        let api = Api::new(cfg).unwrap();
        let url = "https://amp-api.music.apple.com/v1/me/library/albums?limit=100&offset=0";
        let body = fixture("meta_stale_body.json").to_string();

        // every one of these headers is a miss (refetch + overwrite), and
        // the startup sweep drops what the lookup refuses — no entry may be
        // served as fresh or kept forever
        let bad_headers = [
            // clock-ahead laptop: stored a day in the future
            format!("{{\"stored\":{},\"ttl\":3600}}", unix_now() + 86_400),
            // milliseconds-unit stamp (foreign/pre-project cache writer)
            format!("{{\"stored\":{},\"ttl\":3600}}", (unix_now() + 60) * 1000),
            // wrong types / missing fields / junk
            format!("{{\"stored\":\"{}\",\"ttl\":3600}}", unix_now()),
            "{\"ttl\":3600}".to_string(),
            "{\"stored\":1}".to_string(),
            "not json at all".to_string(),
            String::new(), // empty header line, body only
        ];
        for h in &bad_headers {
            let p = api.meta_path(url).unwrap();
            std::fs::write(&p, format!("{h}\n{body}")).unwrap();
            assert!(api.meta_lookup(url).is_none(), "served fresh: {h:?}");
            clean_meta_cache(&api.cfg);
            assert!(!p.exists(), "sweep kept: {h:?}");
        }

        // a well-formed fresh header is still a hit and survives the sweep
        let p = api.meta_path(url).unwrap();
        std::fs::write(
            &p,
            format!("{{\"stored\":{},\"ttl\":{META_TTL_LIBRARY}}}\n{body}", unix_now()),
        )
        .unwrap();
        assert!(api.meta_lookup(url).is_some());
        clean_meta_cache(&api.cfg);
        assert!(p.exists(), "well-formed fresh entry swept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A non-auth network failure (404 here) must not write the error body
    /// into the cache, and the stale copy it serves must be flagged via
    /// `take_notice` (visible staleness).
    #[test]
    fn no_cache_on_4xx_and_stale_serve_is_flagged() {
        let dir = std::env::temp_dir().join(format!("mordent-meta-4xx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            cache_dir: dir.clone(),
            ..Config::default()
        };
        let api = Api::new(cfg).unwrap();
        // bearer comes from a seeded cache so the test never scrapes
        std::fs::write(api.cfg.cache_dir.join("bearer.jwt"), "test-bearer").unwrap();

        let old_body = serde_json::to_string(&fixture("meta_stale_body.json")).unwrap();

        // one-shot HTTP server answering 404 with a JSON error body
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            use std::io::Write as _;
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"errors\":[{\"status\":\"404\"}]}",
            )
            .unwrap();
        });

        // the cache key is the full URL string: fetch the loopback URL and
        // seed its stale copy under exactly that key
        let fetch_url = format!("http://127.0.0.1:{port}/v1/catalog/us/artists/404test");
        let p = api.meta_path(&fetch_url).unwrap();
        // seed a long-expired copy (the startup sweep would have dropped
        // it; this test calls the Api directly, so it is still on disk)
        std::fs::write(
            &p,
            format!("{{\"stored\":0,\"ttl\":{}}}\n{old_body}", META_TTL_CATALOG),
        )
        .unwrap();

        let v = api
            .get_json_retry(&fetch_url)
            .expect("stale fallback should serve the old copy");
        server.join().unwrap();

        assert_eq!(v, fixture("meta_stale_body.json"), "stale copy served");
        // the 404 body never entered the cache; the old entry is untouched
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("\"stored\":0"), "error body must not refresh the header");
        assert!(raw.ends_with(&old_body), "error body must not overwrite the data");
        // the serve is visible, exactly once
        let notice = api.take_notice().expect("stale serve must set a notice");
        assert!(notice.starts_with("showing cached catalog from "), "{notice}");
        assert!(notice.ends_with("— offline?"), "{notice}");
        assert!(api.take_notice().is_none(), "notice consumed once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_error_message_points_at_the_media_user_token() {
        let dir = std::env::temp_dir().join(format!("mordent-meta-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let with_token = Config {
            cache_dir: dir.clone(),
            media_user_token: Some("tok".into()),
            ..Config::default()
        };
        let api = Api::new(with_token).unwrap();
        let e = api.auth_error(anyhow::Error::msg("401"));
        assert_eq!(e.to_string(), "media-user-token expired — update it in the config");

        let no_token = Config {
            cache_dir: dir.clone(),
            media_user_token: None,
            ..Config::default()
        };
        let api = Api::new(no_token).unwrap();
        let e = api.auth_error(anyhow::Error::msg("401 from https://x"));
        assert!(
            e.to_string()
                .starts_with("Apple Music authentication failed"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Load a recorded, trimmed API response. Offline: fixtures only.
    fn fixture(name: &str) -> serde_json::Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let txt = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&txt).unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    fn decoded<T: DeserializeOwned>(name: &str) -> T {
        from_value(fixture(name)).unwrap_or_else(|e| panic!("decode {name}: {e}"))
    }

    #[test]
    fn search_fixture_parses_tracks_albums_artists() {
        let raw: RawSearch = decoded("search_daft_punk.json");
        let songs = &raw.results.songs.data;
        assert_eq!(songs.len(), 2);
        assert_eq!(songs[0].attributes.name, "Get Lucky");
        let track = songs[0].to_track();
        assert_eq!(track.id, "617154366");
        assert_eq!(track.artist, "Daft Punk, Pharrell Williams & Nile Rodgers");
        assert!(track.duration_ms > 0);

        let albums = &raw.results.albums.data;
        assert_eq!(albums.len(), 1);
        let album = albums[0].to_album();
        assert_eq!(album.name, "Starboy");
        assert_eq!(album.track_count, 18);
        assert!(album.artwork.as_deref().unwrap().ends_with("600x600bb.jpg"));

        let artists = &raw.results.artists.data;
        assert_eq!(artists.len(), 1);
        let artist = artists[0].to_artist();
        assert_eq!(artist.name, "Daft Punk");
        assert_eq!(artist.genres, "Dance");
        assert!(artist.artwork_obj.is_some());
    }

    #[test]
    fn catalog_album_id_predicate() {
        assert!(is_catalog_album_id("1440857781"));
        // library ids are not catalog ids (catalog endpoints 404 on them)
        assert!(!is_catalog_album_id("l.pxdMFlY"));
        assert!(!is_catalog_album_id("i.0abc"));
        assert!(!is_catalog_album_id(""));
    }

    #[test]
    fn album_tracks_fixture_parses_and_reports_next_offset() {
        let raw: RawGroup = decoded("album_tracks.json");
        assert_eq!(raw.data.len(), 3);
        assert_eq!(raw.data[0].attributes.name, "Starboy (feat. Daft Punk)");
        assert_eq!(raw.data[0].attributes.track_number, 1);
        assert_eq!(raw.data[0].attributes.disc_number, 1);
        assert_eq!(
            raw.next.as_deref().and_then(parse_next_offset),
            Some(3)
        );
    }

    #[test]
    fn artist_fixture_parses_detail_and_top_songs() {
        let raw: RawArtistResp = decoded("artist.json");
        let a = &raw.data[0];
        assert_eq!(a.id, "5468295");
        assert_eq!(a.attributes.name, "Daft Punk");
        assert_eq!(a.attributes.genre_names, vec!["Dance"]);
        assert_eq!(a.views.top_songs.data.len(), 3);
        let top: Vec<Track> = a.views.top_songs.data.iter().map(|r| r.to_track()).collect();
        assert_eq!(top[1].name, "Get Lucky");
        assert_eq!(top[1].album, "Random Access Memories");
        assert_eq!(
            a.views.top_songs.next.as_deref().and_then(parse_next_offset),
            Some(10)
        );

        let art = a.artwork_obj().expect("artist artwork");
        assert_eq!(art.default_crop_code.as_deref(), Some("bb"));
        assert_eq!(art.width, Some(2909));
        assert!(art.url(1000, 1000).ends_with("1000x1000bb.jpg"));
    }

    #[test]
    fn library_playlists_fixture_parses_optional_fields() {
        let raw: RawLibraryResp = decoded("library_playlists.json");
        let lists: Vec<LibraryPlaylist> = raw.data.iter().map(|r| r.to_playlist()).collect();
        assert_eq!(lists.len(), 2);

        assert_eq!(lists[0].id, "p.FIXTURE00000001");
        assert_eq!(lists[0].name, "Road Trip");
        assert_eq!(lists[0].track_count, 12);
        assert_eq!(lists[0].description, "Public-shaped fixture playlist used by offline tests.");
        let art = lists[0].artwork.as_ref().expect("playlist artwork");
        assert!(art.url(64, 64).ends_with("64x64bb.jpg"));

        // Optional fields absent → defaults, not a parse failure.
        assert_eq!(lists[1].name, "No Artwork, No Description");
        assert_eq!(lists[1].track_count, 3);
        assert_eq!(lists[1].description, "");
        assert!(lists[1].artwork.is_none());

        assert_eq!(raw.next.as_deref().and_then(parse_next_offset), Some(2));
    }

    #[test]
    fn library_albums_fixture_parses_date_added_and_paginates() {
        let raw: RawLibraryResp = decoded("library_albums.json");
        let albums: Vec<AlbumHit> = raw.data.iter().map(|r| r.to_album()).collect();
        assert_eq!(albums.len(), 2);

        assert_eq!(albums[0].id, "528ismsiL5S2WByYmm6b-9v"); // catalog id
        assert_eq!(albums[0].name, "Discovery");
        assert_eq!(albums[0].track_count, 14);
        assert_eq!(albums[0].release_date, "2001-03-07");
        assert_eq!(albums[0].date_added.as_deref(), Some("2026-01-15T09:30:00Z"));
        assert!(albums[0].artwork.is_some());

        // dateAdded/artwork absent → None, not a parse failure.
        assert_eq!(albums[1].name, "No Date, No Artwork");
        assert_eq!(albums[1].date_added, None);
        assert!(albums[1].artwork.is_none());

        assert_eq!(raw.next.as_deref().and_then(parse_next_offset), Some(2));
    }

    #[test]
    fn library_albums_url_includes_sort_param() {
        // default (no sort): unchanged historical URL, server returns its
        // alphabetical-by-title order
        assert_eq!(
            library_albums_url(100, 0, None),
            "https://amp-api.music.apple.com/v1/me/library/albums?limit=100&offset=0&l=en-US"
        );
        // Added → server-side newest-first
        assert_eq!(
            library_albums_url(100, 0, Some("-dateAdded")),
            "https://amp-api.music.apple.com/v1/me/library/albums?limit=100&offset=0&sort=-dateAdded&l=en-US"
        );
        // later pages must carry the same sort so pages tile correctly
        assert_eq!(
            library_albums_url(100, 100, Some("-dateAdded")),
            "https://amp-api.music.apple.com/v1/me/library/albums?limit=100&offset=100&sort=-dateAdded&l=en-US"
        );
    }

    #[test]
    fn library_albums_sorted_fixture_serves_newest_first() {
        // synthetic response shaped like the live sort=-dateAdded page:
        // the first rows are the most recently added albums
        let raw: RawLibraryResp = decoded("library_albums_sorted.json");
        let albums: Vec<AlbumHit> = raw.data.iter().map(|r| r.to_album()).collect();
        assert_eq!(albums.len(), 3);
        let dates: Vec<Option<&str>> = albums.iter().map(|a| a.date_added.as_deref()).collect();
        assert_eq!(
            dates,
            vec![
                Some("2026-10-08T15:15:43Z"),
                Some("2026-04-10T03:01:52Z"),
                Some("2013-11-15T18:51:17Z"),
            ]
        );
        // newest first as served (the UI's dateAdded re-sort is a no-op here)
        assert_eq!(albums[0].name, "Newest Arrival");
        assert_eq!(albums[2].name, "Oldest Arrival");
        // end of pagination
        assert_eq!(raw.next.as_deref().and_then(parse_next_offset), None);
    }

    #[test]
    fn playlist_tracks_fixture_parses_and_paginates() {
        let raw: RawGroup = decoded("playlist_tracks.json");
        let tracks: Vec<Track> = raw.data.iter().map(|r| r.to_track()).collect();
        assert_eq!(tracks.len(), 3);
        // library ids resolve to the catalog id carried in playParams.
        assert_eq!(tracks[0].id, "69719099");
        assert_eq!(tracks[0].name, "One More Time");
        assert_eq!(tracks[0].artist, "Daft Punk");
        assert_eq!(tracks[0].album, "Discovery");
        assert_eq!(tracks[0].duration_ms, 224693);
        assert!(tracks[0].lossless);
        assert!(!tracks[1].lossless);
        assert!(tracks[0].artwork.is_some());
        // missing artwork must not fail decoding.
        assert!(tracks[1].artwork.is_none());
        assert_eq!(raw.next.as_deref().and_then(parse_next_offset), Some(3));
    }

    #[test]
    fn artwork_deserializes_with_defaults() {
        let art: Artwork = from_value(serde_json::json!({
            "url": "https://example.test/{w}x{h}bb.jpg",
            "width": 1500,
        }))
        .unwrap();
        assert_eq!(art.width, Some(1500));
        assert_eq!(art.height, None);
        assert_eq!(art.default_crop_code, None);
        assert!(!art.has_p3);
        assert_eq!(art.url(300, 300), "https://example.test/300x300bb.jpg");
    }

    #[test]
    fn artwork_renders_templates_and_rendered_urls() {
        let template = Artwork {
            url: "https://is1.example/image/{w}x{h}bb.jpg".into(),
            ..Default::default()
        };
        assert_eq!(
            template.url(600, 600),
            "https://is1.example/image/600x600bb.jpg"
        );
        assert_eq!(
            template.url(120, 90),
            "https://is1.example/image/120x90bb.jpg"
        );

        // {c}/{f} placeholders honour defaultCropCode / format defaults.
        let coded = Artwork {
            url: "https://is1.example/image/{w}x{h}{c}.{f}".into(),
            default_crop_code: Some("sr".into()),
            ..Default::default()
        };
        assert_eq!(
            coded.url(60, 60),
            "https://is1.example/image/60x60sr.jpg"
        );
        // jam/shared-listening square fit forces the letterbox crop.
        assert_eq!(
            coded.fit_url(60),
            "https://is1.example/image/60x60bb.jpg"
        );

        // Percent-encoded placeholders (some payloads escape the braces).
        let encoded = Artwork {
            url: "https://is1.example/image/%7Bw%7Dx%7Bh%7Dbf.jpg".into(),
            ..Default::default()
        };
        assert_eq!(
            encoded.url(50, 50),
            "https://is1.example/image/50x50bf.jpg"
        );

        // Already-rendered URLs still resize (and fit can swap the crop code).
        let rendered = Artwork {
            url: "https://is1.example/image/600x600sr.jpg".into(),
            ..Default::default()
        };
        assert_eq!(
            rendered.url(1200, 1200),
            "https://is1.example/image/1200x1200sr.jpg"
        );
        assert_eq!(
            rendered.fit_url(1200),
            "https://is1.example/image/1200x1200bb.jpg"
        );

        // Empty template stays empty rather than panicking.
        assert_eq!(Artwork::default().url(10, 10), "");
    }

    #[test]
    fn track_artwork_url_resizes_stored_url() {
        let t = Track {
            id: "1".into(),
            name: "n".into(),
            artist: "a".into(),
            album: "al".into(),
            duration_ms: 1,
            track_no: 1,
            disc_no: 1,
            artwork: Some("https://is1.example/image/600x600bb.jpg".into()),
            lossless: false,
        };
        assert_eq!(
            t.artwork_url(100, 100).as_deref(),
            Some("https://is1.example/image/100x100bb.jpg")
        );
        assert_eq!(Track { artwork: None, ..t }.artwork_url(10, 10), None);
    }

    #[test]
    fn api_url_absolutizes_relative_paths() {
        assert_eq!(
            api_url("/v1/catalog/us/artists/1/view/top-songs?offset=10"),
            "https://amp-api.music.apple.com/v1/catalog/us/artists/1/view/top-songs?offset=10"
        );
        assert_eq!(
            api_url("https://amp-api.music.apple.com/x"),
            "https://amp-api.music.apple.com/x"
        );
    }

    #[test]
    fn parse_next_offset_handles_empty_and_missing() {
        assert_eq!(parse_next_offset("/v1/me/library/playlists?l=en-US&offset=42"), Some(42));
        assert_eq!(parse_next_offset("/v1/me/library/playlists?offset="), None);
        assert_eq!(parse_next_offset("/v1/me/library/playlists"), None);
    }

    /// Deterministic PLAY_START fixture with the exact observed wire values.
    fn start_fixture() -> PlayEvent {
        PlayEvent {
            build_version: REPORT_BUILD_VERSION.into(),
            developer_token: "dev.jwt".into(),
            event_type: PlayEvent::EVENT_TYPE_PLAY_START,
            ids: PlayIds {
                subscription_adam_id: "1440857781".into(),
                cloud_id: None,
            },
            internal_build: false,
            media_duration_ms: 213_000,
            media_type: 0,
            milliseconds_since_play: 1,
            offline: false,
            persistent_id: "a1b2c3d4e5f60718".into(),
            play_mode: PlayMode { auto: 1, repeat: 1, shuffle: 1 },
            private_enabled: false,
            sb_enabled: true,
            siri_initiated: false,
            source_type: 16,
            start_position_ms: 0,
            store_front: "us".into(),
            item_type: 1,
            user_agent: REPORT_USER_AGENT.into(),
            user_token: "muser".into(),
            utc_offset_seconds: -18_000,
            event_reason_hint_type: Some(PlayEvent::HINT_CONTAINER_CHANGED),
            end_position_ms: None,
            end_reason_type: None,
            container_type: PlayEvent::CONTAINER_ALBUM,
            container_ids: Some(ContainerIds { album_adam_id: "1440857780".into() }),
        }
    }

    #[test]
    fn play_start_serializes_exact_hyphenated_key_set() {
        let v = serde_json::to_value(start_fixture()).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "build-version",
                "container-ids",
                "container-type",
                "developer-token",
                "event-reason-hint-type",
                "event-type",
                "ids",
                "internal-build",
                "media-duration-in-milliseconds",
                "media-type",
                "milliseconds-since-play",
                "offline",
                "persistent-id",
                "play-mode",
                "private-enabled",
                "sb-enabled",
                "siri-initiated",
                "source-type",
                "start-position-in-milliseconds",
                "store-front",
                "type",
                "user-agent",
                "user-token",
                "utc-offset-in-seconds",
            ]
        );
        assert_eq!(v["event-type"], 1);
        assert_eq!(v["event-reason-hint-type"], 1);
        assert_eq!(v["ids"]["subscription-adam-id"], "1440857781"); // string, not number
        assert!(v["ids"].get("cloud-id").is_none());
        assert_eq!(v["container-type"], 3);
        assert_eq!(v["container-ids"]["album-adam-id"], "1440857780");
        assert_eq!(
            v["play-mode"],
            serde_json::json!({"auto-play-mode": 1, "repeat-play-mode": 1, "shuffle-play-mode": 1})
        );
        assert_eq!(v["type"], 1);
        assert_eq!(v["source-type"], 16);
        assert_eq!(v["media-type"], 0);
        assert_eq!(v["utc-offset-in-seconds"], -18_000); // seconds east (US = negative)
        assert_eq!(v["sb-enabled"], true);
        assert_eq!(v["internal-build"], false);
        assert_eq!(v["persistent-id"], "a1b2c3d4e5f60718");
    }

    #[test]
    fn play_end_serializes_end_fields_and_drops_start_only_ones() {
        let mut ev = start_fixture();
        ev.event_type = PlayEvent::EVENT_TYPE_PLAY_END;
        ev.event_reason_hint_type = None;
        ev.end_position_ms = Some(213_000);
        ev.end_reason_type = Some(PlayEvent::END_NATURAL);
        ev.container_type = PlayEvent::CONTAINER_NONE;
        ev.container_ids = None;
        let v = serde_json::to_value(&ev).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("end-position-in-milliseconds"));
        assert!(obj.contains_key("end-reason-type"));
        assert!(!obj.contains_key("event-reason-hint-type"));
        assert!(!obj.contains_key("container-ids")); // unknown container → omitted
        assert_eq!(v["event-type"], 0);
        assert_eq!(v["end-position-in-milliseconds"], 213_000);
        assert_eq!(v["end-reason-type"], 7);
        assert_eq!(v["container-type"], 0);
    }

    #[test]
    fn play_event_builders_produce_start_and_end_shapes() {
        let mode = PlayMode { auto: 1, repeat: 3, shuffle: 2 };
        let start = PlayEvent::start("69719099", 224_693, 0, PlayEvent::HINT_CONTAINER_CHANGED, mode.clone(), None);
        assert_eq!(start.event_type, PlayEvent::EVENT_TYPE_PLAY_START);
        assert_eq!(start.event_reason_hint_type, Some(1));
        assert_eq!(start.end_reason_type, None);
        assert_eq!(start.start_position_ms, 0);
        // no album context → container-type 0, no container-ids
        let v = serde_json::to_value(&start).unwrap();
        assert_eq!(v["container-type"], 0);
        assert!(v.get("container-ids").is_none());

        let end = PlayEvent::end("69719099", 224_693, 5_000, 224_693, PlayEvent::END_NATURAL, mode, None);
        assert_eq!(end.event_type, PlayEvent::EVENT_TYPE_PLAY_END);
        assert_eq!(end.start_position_ms, 5_000); // chains from the resumed segment
        assert_eq!(end.end_position_ms, Some(224_693));
        let v = serde_json::to_value(&end).unwrap();
        assert!(v.get("event-reason-hint-type").is_none());
        assert_eq!(v["play-mode"]["repeat-play-mode"], 3);
        assert_eq!(v["play-mode"]["shuffle-play-mode"], 2);
    }

    #[test]
    fn report_envelope_wraps_events_as_jsplay() {
        let ev = start_fixture();
        let body = JsPlayBody { client_id: "JSCLIENT", event_type: "JSPLAY", data: std::slice::from_ref(&ev) };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["client_id"], "JSCLIENT");
        assert_eq!(v["event_type"], "JSPLAY");
        assert_eq!(v["data"].as_array().map(Vec::len), Some(1));
        assert_eq!(v["data"][0]["event-type"], 1);
    }
}


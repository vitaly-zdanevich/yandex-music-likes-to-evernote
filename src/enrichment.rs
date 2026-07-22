use std::{
    collections::{HashMap, HashSet},
    io::{self, Write as _},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use reqwest::{
    StatusCode,
    header::{HeaderValue, RETRY_AFTER},
};
use serde::{Deserialize, Deserializer};
use tempfile::Builder as TempFileBuilder;
use tokio::time::sleep;
use tracing::warn;
use url::form_urlencoded;

use crate::audio::TrackAudio;
use crate::yandex::LikedTrack;

const CLIENT_NAME: &str = concat!("yandex-music-likes-to-evernote/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const MUSICBRAINZ_MIN_SCORE: i64 = 90;
const MUSICBRAINZ_DURATION_TOLERANCE_MS: u128 = 7_000;
const ACOUSTID_LOOKUP_URL: &str = "https://api.acoustid.org/v2/lookup";
const ACOUSTID_META: &str = "recordings releasegroups compress";
const ACOUSTID_MIN_SCORE: f64 = 0.80;
const SONGLINK_MAX_ATTEMPTS: usize = 3;
const SONGLINK_BACKOFFS: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];
const SONGLINK_MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const WIKIDATA_API_URL: &str = "https://www.wikidata.org/w/api.php";

/// A note row pointing to an external music or metadata service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalLink {
    /// Human-readable link text shown in Evernote when the row is clickable.
    pub label: String,
    /// Absolute URL or preview URL rendered into the Evernote note.
    pub url: String,
    /// Rendering behavior for this row.
    pub kind: ExternalLinkKind,
}

/// Controls how an [`ExternalLink`] is serialized into Evernote ENML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalLinkKind {
    /// Render as a normal clickable anchor.
    Link,
    /// Render as a blank row between exact IDs and generic searches.
    Separator,
    /// Render as a bare YouTube URL so Evernote clients can create a preview.
    YouTubeEmbed,
}

/// Maps a Wikidata external-id property to the service link it can produce.
struct WikidataExternalIdLink {
    property: &'static str,
    service: ExternalLinkService,
    label: &'static str,
    url_builder: fn(&str) -> Option<String>,
}

impl ExternalLink {
    fn new(label: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            url: url.into(),
            kind: ExternalLinkKind::Link,
        }
    }

    fn separator() -> Self {
        Self {
            label: String::new(),
            url: String::new(),
            kind: ExternalLinkKind::Separator,
        }
    }

    fn youtube_embed(url: impl Into<String>) -> Self {
        Self {
            label: "YouTube video".to_string(),
            url: url.into(),
            kind: ExternalLinkKind::YouTubeEmbed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ExternalLinkService {
    AcoustId,
    AllMusic,
    AmazonMusic,
    AppleMusic,
    Bandcamp,
    Beatport,
    Bing,
    Deezer,
    Discogs,
    DuckDuckGo,
    Genius,
    Google,
    LastFm,
    ListenBrainz,
    Lrclib,
    MusicBrainz,
    Qobuz,
    RuTracker,
    Rutube,
    SecondHandSongs,
    Songlink,
    SoundCloud,
    Spotify,
    TheAudioDb,
    Tidal,
    Vimeo,
    Vk,
    WhoSampled,
    Wikidata,
    Wikipedia,
    Yandex,
    YandexVideo,
    YouTube,
    YouTubeMusic,
}

const WIKIDATA_EXTERNAL_ID_LINKS: &[WikidataExternalIdLink] = &[
    WikidataExternalIdLink {
        property: "P2207",
        service: ExternalLinkService::Spotify,
        label: "Spotify track",
        url_builder: spotify_track_url,
    },
    WikidataExternalIdLink {
        property: "P10110",
        service: ExternalLinkService::AppleMusic,
        label: "Apple Music track",
        url_builder: apple_music_track_url,
    },
    WikidataExternalIdLink {
        property: "P2724",
        service: ExternalLinkService::Deezer,
        label: "Deezer track",
        url_builder: deezer_track_url,
    },
    WikidataExternalIdLink {
        property: "P3040",
        service: ExternalLinkService::SoundCloud,
        label: "SoundCloud",
        url_builder: soundcloud_id_url,
    },
    WikidataExternalIdLink {
        property: "P4578",
        service: ExternalLinkService::Tidal,
        label: "TIDAL track",
        url_builder: tidal_track_url,
    },
    WikidataExternalIdLink {
        property: "P1730",
        service: ExternalLinkService::AllMusic,
        label: "AllMusic song",
        url_builder: allmusic_song_url,
    },
    WikidataExternalIdLink {
        property: "P6079",
        service: ExternalLinkService::Discogs,
        label: "Discogs track",
        url_builder: discogs_track_url,
    },
    WikidataExternalIdLink {
        property: "P6218",
        service: ExternalLinkService::Genius,
        label: "Genius",
        url_builder: genius_page_url,
    },
];

impl ExternalLinkService {
    fn parse(name: &str) -> Option<Self> {
        let normalized = name
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>();
        match normalized.as_str() {
            "acoustid" => Some(Self::AcoustId),
            "allmusic" => Some(Self::AllMusic),
            "amazon" | "amazonmusic" => Some(Self::AmazonMusic),
            "apple" | "applemusic" => Some(Self::AppleMusic),
            "bandcamp" => Some(Self::Bandcamp),
            "beatport" => Some(Self::Beatport),
            "bing" => Some(Self::Bing),
            "deezer" => Some(Self::Deezer),
            "discogs" => Some(Self::Discogs),
            "duckduckgo" | "ddg" => Some(Self::DuckDuckGo),
            "genius" => Some(Self::Genius),
            "google" => Some(Self::Google),
            "lastfm" => Some(Self::LastFm),
            "listenbrainz" => Some(Self::ListenBrainz),
            "lrclib" => Some(Self::Lrclib),
            "musicbrainz" => Some(Self::MusicBrainz),
            "qobuz" => Some(Self::Qobuz),
            "rutracker" => Some(Self::RuTracker),
            "rutube" => Some(Self::Rutube),
            "secondhandsongs" => Some(Self::SecondHandSongs),
            "songlink" | "odesli" => Some(Self::Songlink),
            "soundcloud" => Some(Self::SoundCloud),
            "spotify" => Some(Self::Spotify),
            "theaudiodb" | "audiodb" => Some(Self::TheAudioDb),
            "tidal" => Some(Self::Tidal),
            "vimeo" => Some(Self::Vimeo),
            "vk" | "vkontakte" => Some(Self::Vk),
            "wikidata" => Some(Self::Wikidata),
            "wikipedia" => Some(Self::Wikipedia),
            "yandex" => Some(Self::Yandex),
            "yandexvideo" => Some(Self::YandexVideo),
            "youtube" => Some(Self::YouTube),
            "youtubemusic" | "ytmusic" => Some(Self::YouTubeMusic),
            _ => None,
        }
    }
}

const EXTERNAL_LINK_SERVICE_NAMES: &[&str] = &[
    "acoustid",
    "allmusic",
    "amazonmusic",
    "applemusic",
    "bandcamp",
    "beatport",
    "bing",
    "deezer",
    "discogs",
    "duckduckgo",
    "genius",
    "google",
    "lastfm",
    "listenbrainz",
    "lrclib",
    "musicbrainz",
    "qobuz",
    "rutracker",
    "rutube",
    "secondhandsongs",
    "songlink",
    "soundcloud",
    "spotify",
    "theaudiodb",
    "tidal",
    "vimeo",
    "vk",
    "wikidata",
    "wikipedia",
    "yandex",
    "yandexvideo",
    "youtube",
    "youtubemusic",
];

#[derive(Clone)]
pub struct EnrichmentClient {
    http: reqwest::Client,
    genius_access_token: Option<String>,
    acoustid_api_key: Option<String>,
    songlink_user_country: String,
    // Once Songlink keeps returning 429 after retries, avoid slowing every later
    // track in the same sync run with more calls that are likely to fail.
    songlink_rate_limited: Arc<AtomicBool>,
    // Missing fpcalc is an environment problem, so warn once and skip later
    // AcoustID attempts in the same run.
    acoustid_fpcalc_missing: Arc<AtomicBool>,
    enabled_services: HashSet<ExternalLinkService>,
    disabled_services: HashSet<ExternalLinkService>,
}

impl EnrichmentClient {
    pub fn new(
        genius_access_token: Option<String>,
        songlink_user_country: impl Into<String>,
        acoustid_api_key: Option<String>,
        enabled_external_link_services: impl IntoIterator<Item = String>,
        disabled_external_link_services: impl IntoIterator<Item = String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(CLIENT_NAME)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build enrichment HTTP client")?;
        let enabled_services = parse_external_link_services(enabled_external_link_services)?;
        let disabled_services = parse_external_link_services(disabled_external_link_services)?;
        Ok(Self {
            http,
            genius_access_token,
            acoustid_api_key,
            songlink_user_country: songlink_user_country.into(),
            songlink_rate_limited: Arc::new(AtomicBool::new(false)),
            acoustid_fpcalc_missing: Arc::new(AtomicBool::new(false)),
            enabled_services,
            disabled_services,
        })
    }

    pub async fn links_for(
        &self,
        track: &LikedTrack,
        audio: Option<&TrackAudio>,
    ) -> Vec<ExternalLink> {
        let mut exact_links = self.wikidata_exact_links(track).await.unwrap_or_default();
        let has_wikidata_item = !exact_links.is_empty();
        let mut links = Vec::new();

        links.extend(youtube_links(
            track,
            &self.enabled_services,
            &self.disabled_services,
        ));
        if self.service_enabled(ExternalLinkService::LastFm) {
            links.extend(lastfm_links(track));
        }
        if self.service_enabled(ExternalLinkService::RuTracker) {
            links.extend(rutracker_links(track));
        }
        links.extend(self.musicbrainz_links(track, audio).await);

        if self.service_enabled(ExternalLinkService::Lrclib) {
            links.push(self.lrclib_link(track).await.unwrap_or_else(|| {
                ExternalLink::new("LRCLIB lyrics search", lrclib_search_url(track))
            }));
        }

        if self.service_enabled(ExternalLinkService::Songlink) {
            links.push(self.songlink_link(track).await.unwrap_or_else(|| {
                ExternalLink::new("Songlink/Odesli lookup", songlink_lookup_url(track))
            }));
        }

        links.extend(related_music_service_links(
            track,
            &self.enabled_services,
            &self.disabled_services,
        ));
        links.extend(wikimedia_links(
            track,
            &self.enabled_services,
            &self.disabled_services,
            !has_wikidata_item,
        ));

        if self.service_enabled(ExternalLinkService::Genius) {
            links.push(
                self.genius_link(track).await.unwrap_or_else(|| {
                    ExternalLink::new("Genius search", genius_search_url(track))
                }),
            );
        }

        if exact_links.is_empty() {
            links
        } else {
            if !links.is_empty() {
                exact_links.push(ExternalLink::separator());
            }
            exact_links.extend(links);
            exact_links
        }
    }

    fn service_enabled(&self, service: ExternalLinkService) -> bool {
        external_link_service_enabled(service, &self.enabled_services, &self.disabled_services)
    }

    fn should_try_acoustid(&self) -> bool {
        self.service_enabled(ExternalLinkService::AcoustId)
            && (self.service_enabled(ExternalLinkService::MusicBrainz)
                || self.service_enabled(ExternalLinkService::ListenBrainz)
                || self.service_enabled(ExternalLinkService::TheAudioDb))
    }

    /// Builds MusicBrainz links, preferring AcoustID fingerprint matches when
    /// audio and an API key are available, then falling back to metadata search.
    async fn musicbrainz_links(
        &self,
        track: &LikedTrack,
        audio: Option<&TrackAudio>,
    ) -> Vec<ExternalLink> {
        if self.should_try_acoustid()
            && let Some(mut links) = self.acoustid_musicbrainz_links(audio).await
        {
            if self.service_enabled(ExternalLinkService::MusicBrainz) {
                add_missing_musicbrainz_search_links(&mut links, track);
            }
            return links;
        }

        if !self.service_enabled(ExternalLinkService::MusicBrainz) {
            return Vec::new();
        }

        let mut links = vec![self.musicbrainz_link(track).await.unwrap_or_else(|| {
            ExternalLink::new(
                "MusicBrainz recording search",
                musicbrainz_search_url(track),
            )
        })];
        links.extend(musicbrainz_entity_search_links(track));
        links
    }

    /// Looks up a Wikidata item by exact Yandex Music track ID (P13289), then
    /// converts supported claims on that item into direct service links.
    async fn wikidata_exact_links(&self, track: &LikedTrack) -> Option<Vec<ExternalLink>> {
        if !self.service_enabled(ExternalLinkService::Wikidata) {
            return None;
        }

        let item_id = self.wikidata_item_id_for_yandex_track(&track.id).await?;
        let mut links = vec![ExternalLink::new(
            "Wikidata item",
            wikidata_item_url(&item_id),
        )];

        let claims = match self.wikidata_claims_for_item(&item_id).await {
            Some(claims) => claims,
            None => return Some(links),
        };

        let item_labels = self
            .wikidata_item_labels(wikidata_related_item_ids(&claims))
            .await;
        links.extend(wikidata_links_from_claims(
            &claims,
            &item_labels,
            &self.enabled_services,
            &self.disabled_services,
        ));
        Some(links)
    }

    /// Searches Wikidata for an item with Yandex Music track ID (P13289).
    async fn wikidata_item_id_for_yandex_track(&self, track_id: &str) -> Option<String> {
        if !valid_yandex_track_id(track_id) {
            return None;
        }

        let url = wikidata_track_item_lookup_url(track_id)?;
        let response = match self.http.get(&url).send().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "Wikidata track lookup failed");
                return None;
            }
        };
        if !response.status().is_success() {
            warn!(status = %response.status(), "Wikidata track lookup returned non-success status");
            return None;
        }

        let response = match response.json::<WikidataSearchResponse>().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "Wikidata track lookup returned invalid JSON");
                return None;
            }
        };

        wikidata_track_item_id(&response)
    }

    /// Loads the claims for one Wikidata item so exact external IDs can be used.
    async fn wikidata_claims_for_item(
        &self,
        item_id: &str,
    ) -> Option<HashMap<String, Vec<WikidataStatement>>> {
        let url = wikidata_track_claims_lookup_url(item_id)?;
        let response = match self.http.get(&url).send().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, item_id, "Wikidata item claims lookup failed");
                return None;
            }
        };
        if !response.status().is_success() {
            warn!(
                status = %response.status(),
                item_id,
                "Wikidata item claims lookup returned non-success status"
            );
            return None;
        }

        let response = match response.json::<WikidataEntitiesResponse>().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, item_id, "Wikidata item claims lookup returned invalid JSON");
                return None;
            }
        };

        response
            .entities
            .get(item_id)
            .map(|entity| entity.claims.clone())
    }

    /// Loads English labels for linked Wikidata items used in claim-derived rows.
    async fn wikidata_item_labels(&self, item_ids: Vec<String>) -> HashMap<String, String> {
        if item_ids.is_empty() {
            return HashMap::new();
        }

        let url = wikidata_item_labels_lookup_url(&item_ids);
        let response = match self.http.get(&url).send().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "Wikidata item label lookup failed");
                return HashMap::new();
            }
        };
        if !response.status().is_success() {
            warn!(status = %response.status(), "Wikidata item label lookup returned non-success status");
            return HashMap::new();
        }

        let response = match response.json::<WikidataEntitiesResponse>().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "Wikidata item label lookup returned invalid JSON");
                return HashMap::new();
            }
        };

        response
            .entities
            .into_iter()
            .filter_map(|(item_id, entity)| {
                let label = wikidata_entity_label(&entity)?;
                Some((item_id, label))
            })
            .collect()
    }

    /// Uses AcoustID to resolve downloaded audio to exact MusicBrainz entities.
    async fn acoustid_musicbrainz_links(
        &self,
        audio: Option<&TrackAudio>,
    ) -> Option<Vec<ExternalLink>> {
        let api_key = self.acoustid_api_key.as_ref()?;
        let audio = audio?;

        if self.acoustid_fpcalc_missing.load(Ordering::Relaxed) {
            return None;
        }

        let fingerprint = match acoustid_fingerprint(audio) {
            Ok(Some(fingerprint)) => fingerprint,
            Ok(None) => {
                self.acoustid_fpcalc_missing.store(true, Ordering::Relaxed);
                warn!(
                    "fpcalc is not installed; skipping AcoustID lookups for the rest of this run"
                );
                return None;
            }
            Err(error) => {
                warn!(
                    error = format!("{error:#}"),
                    "AcoustID fingerprinting failed"
                );
                return None;
            }
        };

        let duration = fingerprint.duration_seconds.to_string();
        let response = match self
            .http
            .post(ACOUSTID_LOOKUP_URL)
            .form(&[
                ("client", api_key.as_str()),
                ("meta", ACOUSTID_META),
                ("duration", duration.as_str()),
                ("fingerprint", fingerprint.fingerprint.as_str()),
            ])
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "AcoustID lookup failed");
                return None;
            }
        };

        if !response.status().is_success() {
            warn!(status = %response.status(), "AcoustID lookup returned non-success status");
            return None;
        }

        let response = match response.json::<AcoustIdResponse>().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "AcoustID lookup returned invalid JSON");
                return None;
            }
        };

        if response.status != "ok" {
            warn!(
                status = response.status,
                message = response
                    .error
                    .as_ref()
                    .map(|error| error.message.as_str())
                    .unwrap_or("unknown"),
                "AcoustID lookup returned an error"
            );
            return None;
        }

        let recording = best_acoustid_recording(response)?;
        let links = musicbrainz_links_from_acoustid_recording(
            &recording,
            &self.enabled_services,
            &self.disabled_services,
        );
        if links.is_empty() { None } else { Some(links) }
    }

    async fn musicbrainz_link(&self, track: &LikedTrack) -> Option<ExternalLink> {
        let url = musicbrainz_api_url(track);
        let response = match self.http.get(&url).send().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "MusicBrainz lookup failed");
                return None;
            }
        };
        if !response.status().is_success() {
            warn!(status = %response.status(), "MusicBrainz lookup returned non-success status");
            return None;
        }

        let response = match response.json::<MusicBrainzResponse>().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "MusicBrainz lookup returned invalid JSON");
                return None;
            }
        };

        response
            .recordings
            .into_iter()
            .find(|recording| musicbrainz_recording_matches(recording, track))
            .map(|recording| {
                ExternalLink::new(
                    "MusicBrainz recording",
                    format!("https://musicbrainz.org/recording/{}", recording.id),
                )
            })
    }

    async fn lrclib_link(&self, track: &LikedTrack) -> Option<ExternalLink> {
        let url = lrclib_get_url(track);
        let response = match self.http.get(&url).send().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "LRCLIB lookup failed");
                return None;
            }
        };

        if response.status().is_success() {
            Some(ExternalLink::new("LRCLIB lyrics", url))
        } else {
            None
        }
    }

    async fn songlink_link(&self, track: &LikedTrack) -> Option<ExternalLink> {
        if self.songlink_rate_limited.load(Ordering::Relaxed) {
            return None;
        }

        let url = songlink_api_url(track, &self.songlink_user_country);
        let response = self.songlink_response_with_retries(&url).await?;

        let response = match response.json::<SonglinkResponse>().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "Songlink/Odesli lookup returned invalid JSON");
                return None;
            }
        };

        response
            .page_url
            .filter(|url| !url.trim().is_empty())
            .map(|url| ExternalLink::new("Songlink/Odesli", url))
    }

    /// Calls Songlink and waits through short 429 bursts before giving up.
    ///
    /// A successful response is returned to the caller for JSON parsing. Other
    /// failures are non-fatal for sync: the note will still use the fallback
    /// Songlink lookup URL.
    async fn songlink_response_with_retries(&self, url: &str) -> Option<reqwest::Response> {
        for attempt in 1..=SONGLINK_MAX_ATTEMPTS {
            let response = match self.http.get(url).send().await {
                Ok(response) => response,
                Err(error) => {
                    warn!(error = %error, attempt, "Songlink/Odesli lookup failed");
                    return None;
                }
            };

            if response.status().is_success() {
                return Some(response);
            }

            let status = response.status();
            if status != StatusCode::TOO_MANY_REQUESTS {
                warn!(status = %status, "Songlink/Odesli lookup returned non-success status");
                return None;
            }

            if attempt == SONGLINK_MAX_ATTEMPTS {
                self.songlink_rate_limited.store(true, Ordering::Relaxed);
                warn!(
                    status = %status,
                    attempts = attempt,
                    "Songlink/Odesli lookup stayed rate-limited after retries; skipping Songlink for the rest of this run"
                );
                return None;
            }

            let delay = songlink_retry_delay(response.headers().get(RETRY_AFTER), attempt - 1);
            warn!(
                status = %status,
                attempt,
                max_attempts = SONGLINK_MAX_ATTEMPTS,
                retry_after_seconds = delay.as_secs(),
                "Songlink/Odesli lookup rate-limited; backing off"
            );
            sleep(delay).await;
        }

        None
    }

    async fn genius_link(&self, track: &LikedTrack) -> Option<ExternalLink> {
        let token = self.genius_access_token.as_ref()?;
        let url = genius_api_url(track);
        let response = match self.http.get(&url).bearer_auth(token).send().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "Genius lookup failed");
                return None;
            }
        };
        if !response.status().is_success() {
            warn!(status = %response.status(), "Genius lookup returned non-success status");
            return None;
        }

        let response = match response.json::<GeniusResponse>().await {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "Genius lookup returned invalid JSON");
                return None;
            }
        };

        response
            .response
            .hits
            .into_iter()
            .filter_map(|hit| hit.result.url)
            .find(|url| !url.trim().is_empty())
            .map(|url| ExternalLink::new("Genius", url))
    }
}

fn parse_external_link_services(
    names: impl IntoIterator<Item = String>,
) -> Result<HashSet<ExternalLinkService>> {
    let mut services = HashSet::new();
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let service = ExternalLinkService::parse(trimmed).ok_or_else(|| {
            anyhow!(
                "unknown external link service `{trimmed}`; known values: {}",
                EXTERNAL_LINK_SERVICE_NAMES.join(", ")
            )
        })?;
        services.insert(service);
    }
    Ok(services)
}

fn external_link_service_enabled(
    service: ExternalLinkService,
    enabled_services: &HashSet<ExternalLinkService>,
    disabled_services: &HashSet<ExternalLinkService>,
) -> bool {
    (enabled_services.is_empty() || enabled_services.contains(&service))
        && !disabled_services.contains(&service)
}

/// Picks the next Songlink retry delay, preferring a valid Retry-After header.
fn songlink_retry_delay(retry_after: Option<&HeaderValue>, backoff_index: usize) -> Duration {
    retry_after
        .and_then(parse_retry_after_seconds)
        .unwrap_or_else(|| songlink_backoff(backoff_index))
}

/// Parses the numeric Retry-After form used by rate-limit responses.
fn parse_retry_after_seconds(header: &HeaderValue) -> Option<Duration> {
    let seconds = header.to_str().ok()?.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds).min(SONGLINK_MAX_RETRY_AFTER))
}

/// Returns the configured Songlink backoff for an attempt, reusing the last
/// value if more attempts are added without extending the backoff table.
fn songlink_backoff(index: usize) -> Duration {
    SONGLINK_BACKOFFS
        .get(index)
        .copied()
        .unwrap_or(*SONGLINK_BACKOFFS.last().expect("songlink backoff exists"))
}

fn musicbrainz_recording_matches(recording: &MusicBrainzRecording, track: &LikedTrack) -> bool {
    if recording.score < MUSICBRAINZ_MIN_SCORE {
        return false;
    }

    if let (Some(expected), Some(actual)) = (track.duration_ms, recording.length) {
        let diff = expected.abs_diff(actual as u128);
        if diff > MUSICBRAINZ_DURATION_TOLERANCE_MS {
            return false;
        }
    }

    true
}

/// Adds metadata search links for MusicBrainz entities that were not resolved
/// exactly through AcoustID.
fn add_missing_musicbrainz_search_links(links: &mut Vec<ExternalLink>, track: &LikedTrack) {
    let has_artist = links.iter().any(|link| link.label == "MusicBrainz artist");
    let has_album = links.iter().any(|link| link.label == "MusicBrainz album");

    for link in musicbrainz_entity_search_links(track) {
        match link.label.as_str() {
            "MusicBrainz artist search" if !has_artist => links.push(link),
            "MusicBrainz album search" if !has_album => links.push(link),
            _ => {}
        }
    }
}

/// Runs Chromaprint's `fpcalc` helper over the downloaded audio and returns the
/// fingerprint fields required by the AcoustID lookup API.
fn acoustid_fingerprint(audio: &TrackAudio) -> Result<Option<AcoustIdFingerprint>> {
    if audio.bytes.is_empty() {
        return Err(anyhow!("downloaded audio is empty"));
    }

    let mut file = TempFileBuilder::new()
        .suffix(&format!(".{}", audio.extension()))
        .tempfile()
        .context("failed to create temporary audio file for AcoustID fingerprinting")?;
    file.write_all(&audio.bytes)
        .context("failed to write temporary audio file for AcoustID fingerprinting")?;
    file.flush()
        .context("failed to flush temporary audio file for AcoustID fingerprinting")?;

    let output = match Command::new("fpcalc")
        .arg("-json")
        .arg(file.path())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to run fpcalc"),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "fpcalc exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let output = serde_json::from_slice::<FpcalcOutput>(&output.stdout)
        .context("failed to parse fpcalc JSON output")?;
    if output.fingerprint.trim().is_empty() {
        return Err(anyhow!("fpcalc returned an empty fingerprint"));
    }
    if !output.duration.is_finite() || output.duration <= 0.0 {
        return Err(anyhow!("fpcalc returned an invalid duration"));
    }

    Ok(Some(AcoustIdFingerprint {
        duration_seconds: output.duration.round() as u64,
        fingerprint: output.fingerprint,
    }))
}

/// Picks the strongest AcoustID result that is confident enough and linked to a
/// MusicBrainz recording.
fn best_acoustid_recording(mut response: AcoustIdResponse) -> Option<AcoustIdRecording> {
    response
        .results
        .sort_by(|left, right| right.score.total_cmp(&left.score));

    response
        .results
        .into_iter()
        .filter(|result| result.score >= ACOUSTID_MIN_SCORE)
        .flat_map(|result| result.recordings)
        .find(|recording| !recording.id.trim().is_empty())
}

/// Converts an AcoustID MusicBrainz match into direct entity links.
fn musicbrainz_links_from_acoustid_recording(
    recording: &AcoustIdRecording,
    enabled_services: &HashSet<ExternalLinkService>,
    disabled_services: &HashSet<ExternalLinkService>,
) -> Vec<ExternalLink> {
    let mut links = Vec::new();

    if !recording.id.trim().is_empty() {
        if external_link_service_enabled(
            ExternalLinkService::MusicBrainz,
            enabled_services,
            disabled_services,
        ) {
            links.push(ExternalLink::new(
                "MusicBrainz recording",
                format!("https://musicbrainz.org/recording/{}", recording.id),
            ));
        }
        if external_link_service_enabled(
            ExternalLinkService::ListenBrainz,
            enabled_services,
            disabled_services,
        ) {
            links.push(ExternalLink::new(
                "ListenBrainz recording metadata",
                listenbrainz_recording_metadata_url(&recording.id),
            ));
        }
        if external_link_service_enabled(
            ExternalLinkService::TheAudioDb,
            enabled_services,
            disabled_services,
        ) {
            links.push(ExternalLink::new(
                "TheAudioDB track MBID lookup",
                theaudiodb_track_mbid_url(&recording.id),
            ));
        }
    }

    if let Some(artist) = recording
        .artists
        .iter()
        .find(|artist| !artist.id.trim().is_empty())
    {
        if external_link_service_enabled(
            ExternalLinkService::MusicBrainz,
            enabled_services,
            disabled_services,
        ) {
            links.push(ExternalLink::new(
                "MusicBrainz artist",
                format!("https://musicbrainz.org/artist/{}", artist.id),
            ));
        }
        if external_link_service_enabled(
            ExternalLinkService::TheAudioDb,
            enabled_services,
            disabled_services,
        ) {
            links.push(ExternalLink::new(
                "TheAudioDB artist MBID lookup",
                theaudiodb_artist_mbid_url(&artist.id),
            ));
        }
    }

    if let Some(release_group) = recording
        .releasegroups
        .iter()
        .find(|release_group| !release_group.id.trim().is_empty())
    {
        if external_link_service_enabled(
            ExternalLinkService::MusicBrainz,
            enabled_services,
            disabled_services,
        ) {
            links.push(ExternalLink::new(
                "MusicBrainz album",
                format!("https://musicbrainz.org/release-group/{}", release_group.id),
            ));
        }
        if external_link_service_enabled(
            ExternalLinkService::TheAudioDb,
            enabled_services,
            disabled_services,
        ) {
            links.push(ExternalLink::new(
                "TheAudioDB album MBID lookup",
                theaudiodb_album_mbid_url(&release_group.id),
            ));
        }
    }

    links
}

/// Builds extra MusicBrainz entity searches for artist and album context.
fn musicbrainz_entity_search_links(track: &LikedTrack) -> Vec<ExternalLink> {
    let mut links = Vec::new();

    if let Some(artist) = track
        .artists
        .first()
        .filter(|artist| !artist.trim().is_empty())
    {
        links.push(ExternalLink::new(
            "MusicBrainz artist search",
            musicbrainz_artist_search_url(artist),
        ));

        if let Some(album) = track
            .albums
            .first()
            .filter(|album| !album.trim().is_empty())
        {
            links.push(ExternalLink::new(
                "MusicBrainz album search",
                musicbrainz_album_search_url(&format!("{artist} {album}")),
            ));
        }
    }

    links
}

/// Converts validated Wikidata claims into exact links for services the sync
/// already exposes as external-link services.
fn wikidata_links_from_claims(
    claims: &HashMap<String, Vec<WikidataStatement>>,
    item_labels: &HashMap<String, String>,
    enabled_services: &HashSet<ExternalLinkService>,
    disabled_services: &HashSet<ExternalLinkService>,
) -> Vec<ExternalLink> {
    let mut links = Vec::new();

    push_wikidata_item_claim_links(
        &mut links,
        claims,
        item_labels,
        "P921",
        "Wikidata main subject",
    );
    push_wikidata_item_claim_links(
        &mut links,
        claims,
        item_labels,
        "P737",
        "Wikidata influenced by",
    );
    push_wikidata_item_claim_links(&mut links, claims, item_labels, "P407", "Wikidata language");

    if let Some(recording_id) = wikidata_claim_string(claims, "P4404").filter(|id| valid_uuid(id)) {
        if external_link_service_enabled(
            ExternalLinkService::MusicBrainz,
            enabled_services,
            disabled_services,
        ) {
            links.push(ExternalLink::new(
                "MusicBrainz recording",
                musicbrainz_recording_url(&recording_id),
            ));
        }
        if external_link_service_enabled(
            ExternalLinkService::ListenBrainz,
            enabled_services,
            disabled_services,
        ) {
            links.push(ExternalLink::new(
                "ListenBrainz recording metadata",
                listenbrainz_recording_metadata_url(&recording_id),
            ));
        }
        if external_link_service_enabled(
            ExternalLinkService::TheAudioDb,
            enabled_services,
            disabled_services,
        ) {
            links.push(ExternalLink::new(
                "TheAudioDB track MBID lookup",
                theaudiodb_track_mbid_url(&recording_id),
            ));
        }
    }

    if let Some(video_id) = wikidata_claim_string(claims, "P1651").filter(|id| valid_youtube_id(id))
        && external_link_service_enabled(
            ExternalLinkService::YouTube,
            enabled_services,
            disabled_services,
        )
    {
        links.push(ExternalLink::youtube_embed(youtube_video_url(&video_id)));
    }

    for spec in WIKIDATA_EXTERNAL_ID_LINKS {
        if let Some(link) =
            wikidata_external_id_link(claims, spec, enabled_services, disabled_services)
        {
            links.push(link);
        }
    }

    links
}

/// Adds Wikidata item-valued claim links such as main subject and language.
fn push_wikidata_item_claim_links(
    links: &mut Vec<ExternalLink>,
    claims: &HashMap<String, Vec<WikidataStatement>>,
    item_labels: &HashMap<String, String>,
    property: &str,
    label_prefix: &str,
) {
    for item_id in wikidata_claim_item_ids(claims, property) {
        let label = item_labels
            .get(&item_id)
            .map(|label| format!("{label_prefix}: {label}"))
            .unwrap_or_else(|| format!("{label_prefix}: {item_id}"));
        links.push(ExternalLink::new(label, wikidata_item_url(&item_id)));
    }
}

/// Builds one exact external-ID link if the Wikidata claim and service are enabled.
fn wikidata_external_id_link(
    claims: &HashMap<String, Vec<WikidataStatement>>,
    spec: &WikidataExternalIdLink,
    enabled_services: &HashSet<ExternalLinkService>,
    disabled_services: &HashSet<ExternalLinkService>,
) -> Option<ExternalLink> {
    if !external_link_service_enabled(spec.service, enabled_services, disabled_services) {
        return None;
    }

    let value = wikidata_claim_string(claims, spec.property)?;
    let url = (spec.url_builder)(&value)?;
    Some(ExternalLink::new(spec.label, url))
}

/// Returns the related Wikidata item IDs whose labels should be loaded.
fn wikidata_related_item_ids(claims: &HashMap<String, Vec<WikidataStatement>>) -> Vec<String> {
    let mut item_ids = Vec::new();
    for property in ["P921", "P737", "P407"] {
        for item_id in wikidata_claim_item_ids(claims, property) {
            if !item_ids.contains(&item_id) {
                item_ids.push(item_id);
            }
        }
    }
    item_ids
}

fn wikimedia_links(
    track: &LikedTrack,
    enabled_services: &HashSet<ExternalLinkService>,
    disabled_services: &HashSet<ExternalLinkService>,
    include_wikidata_track_search: bool,
) -> Vec<ExternalLink> {
    let mut links = Vec::new();
    let wikidata_enabled = external_link_service_enabled(
        ExternalLinkService::Wikidata,
        enabled_services,
        disabled_services,
    );
    let wikipedia_enabled = external_link_service_enabled(
        ExternalLinkService::Wikipedia,
        enabled_services,
        disabled_services,
    );

    let track_query = if let Some(artist) = track
        .artists
        .first()
        .filter(|artist| !artist.trim().is_empty())
    {
        format!("{artist} {}", track.title)
    } else {
        track.title.clone()
    };
    if wikidata_enabled && include_wikidata_track_search && !track_query.trim().is_empty() {
        links.push(ExternalLink::new(
            "Wikidata track search",
            wikidata_search_url(&track_query),
        ));
    }
    if wikipedia_enabled && !track_query.trim().is_empty() {
        links.push(ExternalLink::new(
            "Wikipedia track search",
            wikipedia_search_url(&track_query),
        ));
    }

    if let Some(artist) = track
        .artists
        .first()
        .filter(|artist| !artist.trim().is_empty())
    {
        if wikidata_enabled {
            links.push(ExternalLink::new(
                "Wikidata artist search",
                wikidata_search_url(artist),
            ));
        }
        if wikipedia_enabled {
            links.push(ExternalLink::new(
                "Wikipedia artist search",
                wikipedia_search_url(artist),
            ));
        }
    }

    if let Some(album) = track
        .albums
        .first()
        .filter(|album| !album.trim().is_empty())
    {
        let query = if let Some(artist) = track.artists.first() {
            format!("{artist} {album}")
        } else {
            album.clone()
        };
        if wikidata_enabled {
            links.push(ExternalLink::new(
                "Wikidata album search",
                wikidata_search_url(&query),
            ));
        }
        if wikipedia_enabled {
            links.push(ExternalLink::new(
                "Wikipedia album search",
                wikipedia_search_url(&query),
            ));
        }
    }

    links
}

fn youtube_links(
    track: &LikedTrack,
    enabled_services: &HashSet<ExternalLinkService>,
    disabled_services: &HashSet<ExternalLinkService>,
) -> Vec<ExternalLink> {
    let query = human_query(track);
    if query.is_empty() {
        return Vec::new();
    }

    let mut links = Vec::new();
    if external_link_service_enabled(
        ExternalLinkService::YouTubeMusic,
        enabled_services,
        disabled_services,
    ) {
        links.push(ExternalLink::new(
            "YouTube Music search",
            youtube_music_search_url(&query),
        ));
    }
    if external_link_service_enabled(
        ExternalLinkService::YouTube,
        enabled_services,
        disabled_services,
    ) {
        links.push(ExternalLink::new(
            "YouTube search",
            youtube_search_url(track),
        ));
    }

    links
}

/// Builds no-token search links for other music catalogs and metadata sites.
fn related_music_service_links(
    track: &LikedTrack,
    enabled_services: &HashSet<ExternalLinkService>,
    disabled_services: &HashSet<ExternalLinkService>,
) -> Vec<ExternalLink> {
    let query = human_query(track);
    if query.is_empty() {
        return Vec::new();
    }

    let mut links = Vec::new();
    let mut push_link = |service, link: ExternalLink| {
        if external_link_service_enabled(service, enabled_services, disabled_services) {
            links.push(link);
        }
    };

    push_link(
        ExternalLinkService::Spotify,
        ExternalLink::new("Spotify search", spotify_search_url(&query)),
    );
    push_link(
        ExternalLinkService::AppleMusic,
        ExternalLink::new("Apple Music search", apple_music_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Deezer,
        ExternalLink::new("Deezer search", deezer_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Bandcamp,
        ExternalLink::new("Bandcamp search", bandcamp_search_url(&query)),
    );
    push_link(
        ExternalLinkService::SoundCloud,
        ExternalLink::new("SoundCloud search", soundcloud_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Discogs,
        ExternalLink::new("Discogs search", discogs_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Tidal,
        ExternalLink::new("TIDAL search", tidal_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Qobuz,
        ExternalLink::new("Qobuz search", qobuz_search_url(&query)),
    );
    push_link(
        ExternalLinkService::AmazonMusic,
        ExternalLink::new("Amazon Music search", amazon_music_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Beatport,
        ExternalLink::new("Beatport search", beatport_search_url(&query)),
    );
    push_link(
        ExternalLinkService::WhoSampled,
        ExternalLink::new("WhoSampled search", whosampled_search_url(&query)),
    );
    push_link(
        ExternalLinkService::SecondHandSongs,
        ExternalLink::new("SecondHandSongs search", secondhandsongs_search_url(&query)),
    );
    push_link(
        ExternalLinkService::AllMusic,
        ExternalLink::new("AllMusic search", allmusic_search_url(&query)),
    );
    push_link(
        ExternalLinkService::ListenBrainz,
        ExternalLink::new("ListenBrainz search", listenbrainz_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Vk,
        ExternalLink::new("VK Music search", vk_music_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Rutube,
        ExternalLink::new("Rutube search", rutube_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Vimeo,
        ExternalLink::new("Vimeo search", vimeo_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Google,
        ExternalLink::new("Google search", google_search_url(&query)),
    );
    push_link(
        ExternalLinkService::DuckDuckGo,
        ExternalLink::new("DuckDuckGo search", duckduckgo_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Bing,
        ExternalLink::new("Bing search", bing_search_url(&query)),
    );
    push_link(
        ExternalLinkService::Yandex,
        ExternalLink::new("Yandex search", yandex_search_url(&query)),
    );

    if external_link_service_enabled(
        ExternalLinkService::TheAudioDb,
        enabled_services,
        disabled_services,
    ) {
        links.extend(theaudiodb_search_links(track));
    }
    if external_link_service_enabled(
        ExternalLinkService::YandexVideo,
        enabled_services,
        disabled_services,
    ) {
        links.extend(yandex_video_links(track));
    }
    links
}

/// Builds public TheAudioDB search API links. TheAudioDB's HTML search pages are
/// not stable, so these links point at its documented JSON search endpoints.
fn theaudiodb_search_links(track: &LikedTrack) -> Vec<ExternalLink> {
    let Some(artist) = track
        .artists
        .first()
        .filter(|artist| !artist.trim().is_empty())
    else {
        return Vec::new();
    };

    let mut links = vec![ExternalLink::new(
        "TheAudioDB artist search",
        theaudiodb_artist_search_url(artist),
    )];

    if !track.title.trim().is_empty() {
        links.push(ExternalLink::new(
            "TheAudioDB track search",
            theaudiodb_track_search_url(artist, &track.title),
        ));
    }

    if let Some(album) = track
        .albums
        .first()
        .filter(|album| !album.trim().is_empty())
    {
        links.push(ExternalLink::new(
            "TheAudioDB album search",
            theaudiodb_album_search_url(artist, album),
        ));
    }

    links
}

/// Builds typed Last.fm searches because it has separate artist, album, and
/// track result pages.
fn lastfm_links(track: &LikedTrack) -> Vec<ExternalLink> {
    let mut links = Vec::new();

    if let Some(artist) = track
        .artists
        .first()
        .filter(|artist| !artist.trim().is_empty())
    {
        links.push(ExternalLink::new(
            "Last.fm artist search",
            lastfm_artist_search_url(artist),
        ));
        links.push(ExternalLink::new(
            "Last.fm track search",
            lastfm_track_search_url(&format!("{artist} {}", track.title)),
        ));

        if let Some(album) = track
            .albums
            .first()
            .filter(|album| !album.trim().is_empty())
        {
            links.push(ExternalLink::new(
                "Last.fm album search",
                lastfm_album_search_url(&format!("{artist} {album}")),
            ));
        }
    } else {
        links.push(ExternalLink::new(
            "Last.fm track search",
            lastfm_track_search_url(&track.title),
        ));
    }

    links
}

/// Builds typed RuTracker searches for artist, track, and album.
fn rutracker_links(track: &LikedTrack) -> Vec<ExternalLink> {
    let mut links = Vec::new();

    if let Some(artist) = track
        .artists
        .first()
        .filter(|artist| !artist.trim().is_empty())
    {
        links.push(ExternalLink::new(
            "RuTracker artist search",
            rutracker_search_url(artist),
        ));
        links.push(ExternalLink::new(
            "RuTracker track search",
            rutracker_search_url(&format!("{artist} {}", track.title)),
        ));

        if let Some(album) = track
            .albums
            .first()
            .filter(|album| !album.trim().is_empty())
        {
            links.push(ExternalLink::new(
                "RuTracker album search",
                rutracker_search_url(&format!("{artist} {album}")),
            ));
        }
    } else {
        links.push(ExternalLink::new(
            "RuTracker track search",
            rutracker_search_url(&track.title),
        ));
    }

    links
}

/// Builds typed Yandex Video searches for track, album, and artist.
fn yandex_video_links(track: &LikedTrack) -> Vec<ExternalLink> {
    let mut links = Vec::new();

    if let Some(artist) = track
        .artists
        .first()
        .filter(|artist| !artist.trim().is_empty())
    {
        links.push(ExternalLink::new(
            "Yandex Video track search",
            yandex_video_search_url(&format!("{artist} {}", track.title)),
        ));

        if let Some(album) = track
            .albums
            .first()
            .filter(|album| !album.trim().is_empty())
        {
            links.push(ExternalLink::new(
                "Yandex Video album search",
                yandex_video_search_url(&format!("{artist} {album}")),
            ));
        }

        links.push(ExternalLink::new(
            "Yandex Video artist search",
            yandex_video_search_url(artist),
        ));
    } else {
        links.push(ExternalLink::new(
            "Yandex Video track search",
            yandex_video_search_url(&track.title),
        ));
    }

    links
}

fn musicbrainz_api_url(track: &LikedTrack) -> String {
    query_url(
        "https://musicbrainz.org/ws/2/recording/",
        &[
            ("query", &musicbrainz_query(track)),
            ("fmt", "json"),
            ("limit", "3"),
        ],
    )
}

fn musicbrainz_search_url(track: &LikedTrack) -> String {
    query_url(
        "https://musicbrainz.org/search",
        &[
            ("query", &human_query(track)),
            ("type", "recording"),
            ("method", "indexed"),
        ],
    )
}

fn musicbrainz_artist_search_url(query: &str) -> String {
    query_url(
        "https://musicbrainz.org/search",
        &[("query", query), ("type", "artist"), ("method", "indexed")],
    )
}

fn musicbrainz_album_search_url(query: &str) -> String {
    query_url(
        "https://musicbrainz.org/search",
        &[
            ("query", query),
            ("type", "release_group"),
            ("method", "indexed"),
        ],
    )
}

fn lrclib_get_url(track: &LikedTrack) -> String {
    let artist = track
        .artists
        .first()
        .map(String::as_str)
        .unwrap_or_default();
    let album = track.albums.first().map(String::as_str).unwrap_or_default();
    let duration = track
        .duration_ms
        .map(|duration| (duration / 1000).to_string());

    let mut params = vec![
        ("track_name", track.title.as_str()),
        ("artist_name", artist),
        ("album_name", album),
    ];
    if let Some(duration) = duration.as_deref() {
        params.push(("duration", duration));
    }

    query_url("https://lrclib.net/api/get", &params)
}

fn lrclib_search_url(track: &LikedTrack) -> String {
    let artist = track
        .artists
        .first()
        .map(String::as_str)
        .unwrap_or_default();
    query_url(
        "https://lrclib.net/api/search",
        &[
            ("track_name", track.title.as_str()),
            ("artist_name", artist),
        ],
    )
}

fn songlink_api_url(track: &LikedTrack, user_country: &str) -> String {
    query_url(
        "https://api.song.link/v1-alpha.1/links",
        &[
            ("url", track.yandex_url.as_str()),
            ("userCountry", user_country),
        ],
    )
}

fn songlink_lookup_url(track: &LikedTrack) -> String {
    query_url(
        "https://api.song.link/v1-alpha.1/links",
        &[("url", track.yandex_url.as_str())],
    )
}

fn wikidata_track_item_lookup_url(track_id: &str) -> Option<String> {
    valid_yandex_track_id(track_id).then(|| {
        query_url(
            WIKIDATA_API_URL,
            &[
                ("format", "json"),
                ("formatversion", "2"),
                ("action", "query"),
                ("list", "search"),
                ("srsearch", &format!("haswbstatement:P13289={track_id}")),
                ("srnamespace", "0"),
                ("srlimit", "1"),
                ("srprop", ""),
            ],
        )
    })
}

fn wikidata_track_claims_lookup_url(item_id: &str) -> Option<String> {
    valid_wikidata_item_id(item_id).then(|| {
        query_url(
            WIKIDATA_API_URL,
            &[
                ("format", "json"),
                ("formatversion", "2"),
                ("action", "wbgetentities"),
                ("ids", item_id),
                ("props", "claims"),
            ],
        )
    })
}

fn wikidata_item_labels_lookup_url(item_ids: &[String]) -> String {
    query_url(
        WIKIDATA_API_URL,
        &[
            ("format", "json"),
            ("formatversion", "2"),
            ("action", "wbgetentities"),
            ("ids", &item_ids.join("|")),
            ("props", "labels"),
            ("languages", "en|ru|uk"),
            ("languagefallback", "1"),
        ],
    )
}

fn wikidata_search_url(query: &str) -> String {
    query_url("https://www.wikidata.org/w/index.php", &[("search", query)])
}

fn wikidata_item_url(item_id: &str) -> String {
    format!("https://www.wikidata.org/wiki/{item_id}")
}

fn wikipedia_search_url(query: &str) -> String {
    query_url("https://en.wikipedia.org/w/index.php", &[("search", query)])
}

fn genius_api_url(track: &LikedTrack) -> String {
    query_url(
        "https://api.genius.com/search",
        &[("q", &human_query(track))],
    )
}

fn genius_search_url(track: &LikedTrack) -> String {
    query_url("https://genius.com/search", &[("q", &human_query(track))])
}

fn spotify_search_url(query: &str) -> String {
    query_url("https://open.spotify.com/search", &[("q", query)])
}

fn spotify_track_url(track_id: &str) -> Option<String> {
    valid_spotify_track_id(track_id).then(|| format!("https://open.spotify.com/track/{track_id}"))
}

fn apple_music_search_url(query: &str) -> String {
    query_url("https://music.apple.com/search", &[("term", query)])
}

fn apple_music_track_url(track_id: &str) -> Option<String> {
    if !valid_apple_music_track_id(track_id) {
        return None;
    }

    if let Some((album_id, item_id)) = track_id.split_once("?i=") {
        return Some(format!(
            "https://geo.music.apple.com/album/id{album_id}?i={item_id}"
        ));
    }

    Some(query_url(
        "https://wikidata-externalid-url.toolforge.org/",
        &[
            ("url_prefix", "https://geo.music.apple.com/album/id"),
            ("id", track_id),
        ],
    ))
}

fn deezer_search_url(query: &str) -> String {
    query_url("https://www.deezer.com/search", &[("q", query)])
}

fn deezer_track_url(track_id: &str) -> Option<String> {
    valid_nonzero_numeric_id(track_id).then(|| format!("https://www.deezer.com/track/{track_id}"))
}

fn bandcamp_search_url(query: &str) -> String {
    query_url("https://bandcamp.com/search", &[("q", query)])
}

fn soundcloud_search_url(query: &str) -> String {
    query_url("https://soundcloud.com/search", &[("q", query)])
}

fn soundcloud_id_url(id: &str) -> Option<String> {
    valid_soundcloud_id(id).then(|| format!("https://soundcloud.com/{id}"))
}

fn discogs_search_url(query: &str) -> String {
    query_url("https://www.discogs.com/search/", &[("q", query)])
}

fn discogs_track_url(track_id: &str) -> Option<String> {
    valid_uuid(track_id)
        .then(|| format!("https://www.discogs.com/track/{}", track_id.to_lowercase()))
}

fn tidal_search_url(query: &str) -> String {
    path_search_url("https://tidal.com/search", query)
}

fn tidal_track_url(track_id: &str) -> Option<String> {
    valid_nonzero_numeric_id(track_id).then(|| format!("https://tidal.com/browse/track/{track_id}"))
}

fn qobuz_search_url(query: &str) -> String {
    path_search_url("https://play.qobuz.com/search", query)
}

fn amazon_music_search_url(query: &str) -> String {
    path_search_url("https://music.amazon.com/search", query)
}

fn youtube_music_search_url(query: &str) -> String {
    query_url("https://music.youtube.com/search", &[("q", query)])
}

fn beatport_search_url(query: &str) -> String {
    query_url("https://www.beatport.com/search", &[("q", query)])
}

fn whosampled_search_url(query: &str) -> String {
    query_url("https://www.whosampled.com/search/", &[("q", query)])
}

fn secondhandsongs_search_url(query: &str) -> String {
    query_url(
        "https://secondhandsongs.com/search",
        &[("search_text", query)],
    )
}

fn allmusic_search_url(query: &str) -> String {
    path_search_url("https://www.allmusic.com/search/all", query)
}

fn allmusic_song_url(song_id: &str) -> Option<String> {
    valid_allmusic_song_id(song_id).then(|| format!("https://www.allmusic.com/song/{song_id}"))
}

fn listenbrainz_search_url(query: &str) -> String {
    query_url("https://listenbrainz.org/search/", &[("q", query)])
}

fn vk_music_search_url(query: &str) -> String {
    query_url(
        "https://vk.com/search",
        &[("c[q]", query), ("c[section]", "audio")],
    )
}

fn rutube_search_url(query: &str) -> String {
    query_url("https://rutube.ru/search/", &[("query", query)])
}

fn rutracker_search_url(query: &str) -> String {
    query_url("https://rutracker.org/forum/tracker.php", &[("nm", query)])
}

fn vimeo_search_url(query: &str) -> String {
    query_url("https://vimeo.com/search", &[("q", query)])
}

fn google_search_url(query: &str) -> String {
    query_url("https://www.google.com/search", &[("q", query)])
}

fn duckduckgo_search_url(query: &str) -> String {
    query_url("https://duckduckgo.com/", &[("q", query)])
}

fn bing_search_url(query: &str) -> String {
    query_url("https://www.bing.com/search", &[("q", query)])
}

fn yandex_search_url(query: &str) -> String {
    query_url("https://yandex.ru/search/", &[("text", query)])
}

fn yandex_video_search_url(query: &str) -> String {
    query_url("https://yandex.ru/video/search", &[("text", query)])
}

fn theaudiodb_artist_search_url(artist: &str) -> String {
    query_url(
        "https://www.theaudiodb.com/api/v1/json/2/search.php",
        &[("s", artist)],
    )
}

fn theaudiodb_track_search_url(artist: &str, title: &str) -> String {
    query_url(
        "https://www.theaudiodb.com/api/v1/json/2/searchtrack.php",
        &[("s", artist), ("t", title)],
    )
}

fn theaudiodb_album_search_url(artist: &str, album: &str) -> String {
    query_url(
        "https://www.theaudiodb.com/api/v1/json/2/searchalbum.php",
        &[("s", artist), ("a", album)],
    )
}

fn theaudiodb_track_mbid_url(recording_mbid: &str) -> String {
    query_url(
        "https://www.theaudiodb.com/api/v1/json/2/track-mb.php",
        &[("i", recording_mbid)],
    )
}

fn theaudiodb_artist_mbid_url(artist_mbid: &str) -> String {
    query_url(
        "https://www.theaudiodb.com/api/v1/json/2/artist-mb.php",
        &[("i", artist_mbid)],
    )
}

fn theaudiodb_album_mbid_url(release_group_mbid: &str) -> String {
    query_url(
        "https://www.theaudiodb.com/api/v1/json/2/album-mb.php",
        &[("i", release_group_mbid)],
    )
}

fn listenbrainz_recording_metadata_url(recording_mbid: &str) -> String {
    query_url(
        "https://api.listenbrainz.org/1/metadata/recording/",
        &[
            ("recording_mbids", recording_mbid),
            ("inc", "artist release"),
        ],
    )
}

fn lastfm_artist_search_url(query: &str) -> String {
    query_url("https://www.last.fm/search/artists", &[("q", query)])
}

fn lastfm_track_search_url(query: &str) -> String {
    query_url("https://www.last.fm/search/tracks", &[("q", query)])
}

fn lastfm_album_search_url(query: &str) -> String {
    query_url("https://www.last.fm/search/albums", &[("q", query)])
}

fn youtube_search_url(track: &LikedTrack) -> String {
    query_url(
        "https://www.youtube.com/results",
        &[("search_query", &human_query(track))],
    )
}

fn youtube_video_url(video_id: &str) -> String {
    query_url("https://www.youtube.com/watch", &[("v", video_id)])
}

fn musicbrainz_recording_url(recording_id: &str) -> String {
    format!(
        "https://musicbrainz.org/recording/{}",
        recording_id.to_lowercase()
    )
}

fn genius_page_url(genius_id: &str) -> Option<String> {
    valid_genius_id(genius_id).then(|| format!("https://genius.com/{genius_id}"))
}

fn musicbrainz_query(track: &LikedTrack) -> String {
    let mut query = format!("recording:\"{}\"", lucene_escape(&track.title));
    if let Some(artist) = track.artists.first() {
        query.push_str(&format!(" AND artist:\"{}\"", lucene_escape(artist)));
    }
    query
}

fn human_query(track: &LikedTrack) -> String {
    let artist = track
        .artists
        .first()
        .map(String::as_str)
        .unwrap_or_default();
    let album = track.albums.first().map(String::as_str).unwrap_or_default();
    [artist, track.title.as_str(), album]
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn lucene_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn query_url(base: &str, params: &[(&str, &str)]) -> String {
    let query = params
        .iter()
        .fold(
            form_urlencoded::Serializer::new(String::new()),
            |mut ser, (key, value)| {
                ser.append_pair(key, value);
                ser
            },
        )
        .finish();
    format!("{base}?{query}")
}

fn path_search_url(base: &str, query: &str) -> String {
    let query = form_urlencoded::byte_serialize(query.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!("{base}/{query}")
}

#[derive(Debug, Deserialize)]
struct MusicBrainzResponse {
    #[serde(default)]
    recordings: Vec<MusicBrainzRecording>,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzRecording {
    id: String,
    #[serde(default, deserialize_with = "deserialize_i64_or_string")]
    score: i64,
    length: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
struct AcoustIdFingerprint {
    duration_seconds: u64,
    fingerprint: String,
}

#[derive(Debug, Deserialize)]
struct FpcalcOutput {
    duration: f64,
    fingerprint: String,
}

#[derive(Debug, Deserialize)]
struct AcoustIdResponse {
    status: String,
    #[serde(default)]
    results: Vec<AcoustIdResult>,
    error: Option<AcoustIdError>,
}

#[derive(Debug, Deserialize)]
struct AcoustIdError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct AcoustIdResult {
    #[serde(default)]
    score: f64,
    #[serde(default)]
    recordings: Vec<AcoustIdRecording>,
}

#[derive(Debug, Deserialize)]
struct AcoustIdRecording {
    id: String,
    #[serde(default)]
    artists: Vec<AcoustIdArtist>,
    #[serde(default)]
    releasegroups: Vec<AcoustIdReleaseGroup>,
}

#[derive(Debug, Deserialize)]
struct AcoustIdArtist {
    id: String,
}

#[derive(Debug, Deserialize)]
struct AcoustIdReleaseGroup {
    id: String,
}

fn deserialize_i64_or_string<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => Ok(number.as_i64().unwrap_or_default()),
        serde_json::Value::String(string) => Ok(string.parse().unwrap_or_default()),
        _ => Ok(0),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SonglinkResponse {
    page_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeniusResponse {
    response: GeniusSearchResponse,
}

#[derive(Debug, Deserialize)]
struct GeniusSearchResponse {
    hits: Vec<GeniusHit>,
}

#[derive(Debug, Deserialize)]
struct GeniusHit {
    result: GeniusHitResult,
}

#[derive(Debug, Deserialize)]
struct GeniusHitResult {
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WikidataSearchResponse {
    query: Option<WikidataSearchQuery>,
}

#[derive(Debug, Deserialize)]
struct WikidataSearchQuery {
    #[serde(default)]
    search: Vec<WikidataSearchResult>,
}

#[derive(Debug, Deserialize)]
struct WikidataSearchResult {
    title: String,
}

#[derive(Debug, Deserialize)]
struct WikidataEntitiesResponse {
    #[serde(default)]
    entities: HashMap<String, WikidataEntity>,
}

#[derive(Debug, Clone, Deserialize)]
struct WikidataEntity {
    #[serde(default)]
    claims: HashMap<String, Vec<WikidataStatement>>,
    #[serde(default)]
    labels: HashMap<String, WikidataLabel>,
}

#[derive(Debug, Clone, Deserialize)]
struct WikidataLabel {
    value: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WikidataStatement {
    mainsnak: WikidataSnak,
    rank: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WikidataSnak {
    snaktype: String,
    datavalue: Option<WikidataDataValue>,
}

#[derive(Debug, Clone, Deserialize)]
struct WikidataDataValue {
    value: serde_json::Value,
}

/// Returns the first valid Q-id from a Wikidata search response.
fn wikidata_track_item_id(response: &WikidataSearchResponse) -> Option<String> {
    response
        .query
        .as_ref()?
        .search
        .iter()
        .map(|result| result.title.trim())
        .find(|title| valid_wikidata_item_id(title))
        .map(ToOwned::to_owned)
}

/// Extracts the first active string-valued claim for a Wikidata property.
fn wikidata_claim_string(
    claims: &HashMap<String, Vec<WikidataStatement>>,
    property: &str,
) -> Option<String> {
    claims.get(property)?.iter().find_map(|statement| {
        let value = active_wikidata_datavalue(statement)?;
        let value = match value {
            serde_json::Value::String(value) => value.trim().to_string(),
            serde_json::Value::Number(value) => value.to_string(),
            _ => return None,
        };
        (!value.is_empty()).then_some(value)
    })
}

/// Extracts unique active item-valued Q-ids for a Wikidata property.
fn wikidata_claim_item_ids(
    claims: &HashMap<String, Vec<WikidataStatement>>,
    property: &str,
) -> Vec<String> {
    let mut item_ids = Vec::new();
    let Some(statements) = claims.get(property) else {
        return item_ids;
    };

    for statement in statements {
        let Some(value) = active_wikidata_datavalue(statement) else {
            continue;
        };
        let Some(item_id) = wikidata_datavalue_item_id(value) else {
            continue;
        };
        if !item_ids.contains(&item_id) {
            item_ids.push(item_id);
        }
    }

    item_ids
}

/// Picks a readable label from Wikidata's language-fallback response.
fn wikidata_entity_label(entity: &WikidataEntity) -> Option<String> {
    ["en", "ru", "uk"]
        .into_iter()
        .filter_map(|language| entity.labels.get(language))
        .chain(entity.labels.values())
        .map(|label| label.value.trim())
        .find(|label| !label.is_empty())
        .map(ToOwned::to_owned)
}

fn active_wikidata_datavalue(statement: &WikidataStatement) -> Option<&serde_json::Value> {
    if statement.rank.as_deref() == Some("deprecated") || statement.mainsnak.snaktype != "value" {
        return None;
    }

    Some(&statement.mainsnak.datavalue.as_ref()?.value)
}

fn wikidata_datavalue_item_id(value: &serde_json::Value) -> Option<String> {
    let object = value.as_object()?;
    if let Some(item_id) = object.get("id").and_then(|id| id.as_str())
        && valid_wikidata_item_id(item_id)
    {
        return Some(item_id.to_string());
    }

    let numeric_id = object.get("numeric-id")?.as_u64()?;
    let item_id = format!("Q{numeric_id}");
    valid_wikidata_item_id(&item_id).then_some(item_id)
}

fn valid_yandex_track_id(track_id: &str) -> bool {
    valid_nonzero_numeric_id(track_id.trim())
}

fn valid_wikidata_item_id(item_id: &str) -> bool {
    item_id
        .strip_prefix('Q')
        .is_some_and(valid_nonzero_numeric_id)
}

fn valid_uuid(value: &str) -> bool {
    let parts = value.split('-').collect::<Vec<_>>();
    let expected_lengths = [8, 4, 4, 4, 12];
    parts.len() == expected_lengths.len()
        && parts
            .iter()
            .zip(expected_lengths)
            .all(|(part, expected_len)| {
                part.len() == expected_len && part.chars().all(|ch| ch.is_ascii_hexdigit())
            })
}

fn valid_youtube_id(value: &str) -> bool {
    value.len() == 11
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn valid_spotify_track_id(value: &str) -> bool {
    value.len() == 22 && value.chars().all(|ch| ch.is_ascii_alphanumeric())
}

fn valid_apple_music_track_id(value: &str) -> bool {
    if let Some((album_id, track_id)) = value.split_once("?i=") {
        return valid_nonzero_numeric_id(album_id) && valid_nonzero_numeric_id(track_id);
    }

    valid_nonzero_numeric_id(value)
}

fn valid_nonzero_numeric_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.bytes().any(|byte| byte != b'0')
}

fn valid_soundcloud_id(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains("//")
        && !value.contains("..")
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '/'))
}

fn valid_allmusic_song_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn valid_genius_id(value: &str) -> bool {
    let starts_correctly = value
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit());
    starts_correctly
        && (value.ends_with("-lyrics") || value.ends_with("-annotated"))
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn sample_track() -> LikedTrack {
        LikedTrack {
            id: "123".to_string(),
            liked_at: chrono::Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap(),
            title: "Song & Name".to_string(),
            artists: vec!["Artist Name".to_string()],
            artist_links: Vec::new(),
            albums: vec!["Album Name".to_string()],
            album_links: Vec::new(),
            duration_ms: Some(123_000),
            cover_url: None,
            yandex_url: "https://music.yandex.com/track/123".to_string(),
        }
    }

    fn no_enabled_services() -> HashSet<ExternalLinkService> {
        HashSet::new()
    }

    fn no_disabled_services() -> HashSet<ExternalLinkService> {
        HashSet::new()
    }

    #[test]
    fn builds_encoded_external_search_urls() {
        let track = sample_track();

        assert_eq!(
            genius_search_url(&track),
            "https://genius.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
        );
        assert_eq!(
            lrclib_get_url(&track),
            "https://lrclib.net/api/get?track_name=Song+%26+Name&artist_name=Artist+Name&album_name=Album+Name&duration=123"
        );
        assert_eq!(
            musicbrainz_entity_search_links(&track),
            vec![
                ExternalLink::new(
                    "MusicBrainz artist search",
                    "https://musicbrainz.org/search?query=Artist+Name&type=artist&method=indexed"
                ),
                ExternalLink::new(
                    "MusicBrainz album search",
                    "https://musicbrainz.org/search?query=Artist+Name+Album+Name&type=release_group&method=indexed"
                ),
            ]
        );
    }

    #[test]
    fn builds_related_music_service_search_links() {
        let track = sample_track();
        let enabled_services = no_enabled_services();
        let disabled_services = no_disabled_services();

        assert_eq!(
            youtube_links(&track, &enabled_services, &disabled_services),
            vec![
                ExternalLink::new(
                    "YouTube Music search",
                    "https://music.youtube.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "YouTube search",
                    "https://www.youtube.com/results?search_query=Artist+Name+Song+%26+Name+Album+Name"
                ),
            ]
        );
        assert_eq!(
            related_music_service_links(&track, &enabled_services, &disabled_services),
            vec![
                ExternalLink::new(
                    "Spotify search",
                    "https://open.spotify.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Apple Music search",
                    "https://music.apple.com/search?term=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Deezer search",
                    "https://www.deezer.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Bandcamp search",
                    "https://bandcamp.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "SoundCloud search",
                    "https://soundcloud.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Discogs search",
                    "https://www.discogs.com/search/?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "TIDAL search",
                    "https://tidal.com/search/Artist%20Name%20Song%20%26%20Name%20Album%20Name"
                ),
                ExternalLink::new(
                    "Qobuz search",
                    "https://play.qobuz.com/search/Artist%20Name%20Song%20%26%20Name%20Album%20Name"
                ),
                ExternalLink::new(
                    "Amazon Music search",
                    "https://music.amazon.com/search/Artist%20Name%20Song%20%26%20Name%20Album%20Name"
                ),
                ExternalLink::new(
                    "Beatport search",
                    "https://www.beatport.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "WhoSampled search",
                    "https://www.whosampled.com/search/?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "SecondHandSongs search",
                    "https://secondhandsongs.com/search?search_text=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "AllMusic search",
                    "https://www.allmusic.com/search/all/Artist%20Name%20Song%20%26%20Name%20Album%20Name"
                ),
                ExternalLink::new(
                    "ListenBrainz search",
                    "https://listenbrainz.org/search/?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "VK Music search",
                    "https://vk.com/search?c%5Bq%5D=Artist+Name+Song+%26+Name+Album+Name&c%5Bsection%5D=audio"
                ),
                ExternalLink::new(
                    "Rutube search",
                    "https://rutube.ru/search/?query=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Vimeo search",
                    "https://vimeo.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Google search",
                    "https://www.google.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "DuckDuckGo search",
                    "https://duckduckgo.com/?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Bing search",
                    "https://www.bing.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Yandex search",
                    "https://yandex.ru/search/?text=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "TheAudioDB artist search",
                    "https://www.theaudiodb.com/api/v1/json/2/search.php?s=Artist+Name"
                ),
                ExternalLink::new(
                    "TheAudioDB track search",
                    "https://www.theaudiodb.com/api/v1/json/2/searchtrack.php?s=Artist+Name&t=Song+%26+Name"
                ),
                ExternalLink::new(
                    "TheAudioDB album search",
                    "https://www.theaudiodb.com/api/v1/json/2/searchalbum.php?s=Artist+Name&a=Album+Name"
                ),
                ExternalLink::new(
                    "Yandex Video track search",
                    "https://yandex.ru/video/search?text=Artist+Name+Song+%26+Name"
                ),
                ExternalLink::new(
                    "Yandex Video album search",
                    "https://yandex.ru/video/search?text=Artist+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Yandex Video artist search",
                    "https://yandex.ru/video/search?text=Artist+Name"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn orders_youtube_lastfm_and_rutracker_first() {
        let track = sample_track();
        let client = EnrichmentClient::new(
            None,
            "US",
            None,
            vec![
                "youtube-music".to_string(),
                "youtube".to_string(),
                "lastfm".to_string(),
                "rutracker".to_string(),
            ],
            Vec::<String>::new(),
        )
        .expect("client");

        assert_eq!(
            client.links_for(&track, None).await,
            vec![
                ExternalLink::new(
                    "YouTube Music search",
                    "https://music.youtube.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "YouTube search",
                    "https://www.youtube.com/results?search_query=Artist+Name+Song+%26+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Last.fm artist search",
                    "https://www.last.fm/search/artists?q=Artist+Name"
                ),
                ExternalLink::new(
                    "Last.fm track search",
                    "https://www.last.fm/search/tracks?q=Artist+Name+Song+%26+Name"
                ),
                ExternalLink::new(
                    "Last.fm album search",
                    "https://www.last.fm/search/albums?q=Artist+Name+Album+Name"
                ),
                ExternalLink::new(
                    "RuTracker artist search",
                    "https://rutracker.org/forum/tracker.php?nm=Artist+Name"
                ),
                ExternalLink::new(
                    "RuTracker track search",
                    "https://rutracker.org/forum/tracker.php?nm=Artist+Name+Song+%26+Name"
                ),
                ExternalLink::new(
                    "RuTracker album search",
                    "https://rutracker.org/forum/tracker.php?nm=Artist+Name+Album+Name"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn applies_whitelist_and_blocklist_to_newer_external_services() {
        let track = sample_track();
        let client = EnrichmentClient::new(
            None,
            "US",
            None,
            vec![
                "last.fm".to_string(),
                "rutracker".to_string(),
                "yandex-video".to_string(),
            ],
            vec!["rutracker".to_string()],
        )
        .expect("client");

        assert_eq!(
            client.links_for(&track, None).await,
            vec![
                ExternalLink::new(
                    "Last.fm artist search",
                    "https://www.last.fm/search/artists?q=Artist+Name"
                ),
                ExternalLink::new(
                    "Last.fm track search",
                    "https://www.last.fm/search/tracks?q=Artist+Name+Song+%26+Name"
                ),
                ExternalLink::new(
                    "Last.fm album search",
                    "https://www.last.fm/search/albums?q=Artist+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Yandex Video track search",
                    "https://yandex.ru/video/search?text=Artist+Name+Song+%26+Name"
                ),
                ExternalLink::new(
                    "Yandex Video album search",
                    "https://yandex.ru/video/search?text=Artist+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Yandex Video artist search",
                    "https://yandex.ru/video/search?text=Artist+Name"
                ),
            ]
        );
    }

    #[test]
    fn filters_weak_musicbrainz_matches() {
        let track = sample_track();
        assert!(musicbrainz_recording_matches(
            &MusicBrainzRecording {
                id: "id".to_string(),
                score: 100,
                length: Some(124_000),
            },
            &track
        ));
        assert!(!musicbrainz_recording_matches(
            &MusicBrainzRecording {
                id: "id".to_string(),
                score: 89,
                length: Some(124_000),
            },
            &track
        ));
        assert!(!musicbrainz_recording_matches(
            &MusicBrainzRecording {
                id: "id".to_string(),
                score: 100,
                length: Some(150_000),
            },
            &track
        ));
    }

    #[test]
    fn deserializes_musicbrainz_string_score() {
        let recording = serde_json::from_str::<MusicBrainzRecording>(
            r#"{"id":"id","score":"100","length":123000}"#,
        )
        .expect("deserialize recording");

        assert_eq!(recording.score, 100);
    }

    #[test]
    fn extracts_musicbrainz_links_from_acoustid_response() {
        let response = serde_json::from_str::<AcoustIdResponse>(
            r#"{
                "status": "ok",
                "results": [
                    {
                        "score": 0.7,
                        "recordings": [{"id": "weak-recording"}]
                    },
                    {
                        "score": 0.98,
                        "recordings": [{
                            "id": "recording-id",
                            "artists": [{"id": "artist-id", "name": "Artist Name"}],
                            "releasegroups": [{"id": "album-id", "title": "Album Name"}]
                        }]
                    }
                ]
            }"#,
        )
        .expect("deserialize AcoustID response");

        let recording = best_acoustid_recording(response).expect("recording match");
        let enabled_services = no_enabled_services();
        let disabled_services = no_disabled_services();

        assert_eq!(
            musicbrainz_links_from_acoustid_recording(
                &recording,
                &enabled_services,
                &disabled_services,
            ),
            vec![
                ExternalLink::new(
                    "MusicBrainz recording",
                    "https://musicbrainz.org/recording/recording-id"
                ),
                ExternalLink::new(
                    "ListenBrainz recording metadata",
                    "https://api.listenbrainz.org/1/metadata/recording/?recording_mbids=recording-id&inc=artist+release"
                ),
                ExternalLink::new(
                    "TheAudioDB track MBID lookup",
                    "https://www.theaudiodb.com/api/v1/json/2/track-mb.php?i=recording-id"
                ),
                ExternalLink::new(
                    "MusicBrainz artist",
                    "https://musicbrainz.org/artist/artist-id"
                ),
                ExternalLink::new(
                    "TheAudioDB artist MBID lookup",
                    "https://www.theaudiodb.com/api/v1/json/2/artist-mb.php?i=artist-id"
                ),
                ExternalLink::new(
                    "MusicBrainz album",
                    "https://musicbrainz.org/release-group/album-id"
                ),
                ExternalLink::new(
                    "TheAudioDB album MBID lookup",
                    "https://www.theaudiodb.com/api/v1/json/2/album-mb.php?i=album-id"
                ),
            ]
        );
    }

    #[test]
    fn ignores_weak_acoustid_matches() {
        let response = serde_json::from_str::<AcoustIdResponse>(
            r#"{
                "status": "ok",
                "results": [{
                    "score": 0.79,
                    "recordings": [{"id": "recording-id"}]
                }]
            }"#,
        )
        .expect("deserialize AcoustID response");

        assert!(best_acoustid_recording(response).is_none());
    }

    #[test]
    fn adds_musicbrainz_search_links_missing_after_acoustid_match() {
        let track = sample_track();
        let mut links = vec![ExternalLink::new(
            "MusicBrainz recording",
            "https://musicbrainz.org/recording/recording-id",
        )];

        add_missing_musicbrainz_search_links(&mut links, &track);

        assert_eq!(
            links,
            vec![
                ExternalLink::new(
                    "MusicBrainz recording",
                    "https://musicbrainz.org/recording/recording-id"
                ),
                ExternalLink::new(
                    "MusicBrainz artist search",
                    "https://musicbrainz.org/search?query=Artist+Name&type=artist&method=indexed"
                ),
                ExternalLink::new(
                    "MusicBrainz album search",
                    "https://musicbrainz.org/search?query=Artist+Name+Album+Name&type=release_group&method=indexed"
                ),
            ]
        );
    }

    #[test]
    fn preserves_exact_acoustid_musicbrainz_entities() {
        let track = sample_track();
        let mut links = vec![
            ExternalLink::new(
                "MusicBrainz artist",
                "https://musicbrainz.org/artist/artist-id",
            ),
            ExternalLink::new(
                "MusicBrainz album",
                "https://musicbrainz.org/release-group/album-id",
            ),
        ];

        add_missing_musicbrainz_search_links(&mut links, &track);

        assert_eq!(links.len(), 2);
    }

    #[test]
    fn parses_external_link_service_aliases() {
        let services = parse_external_link_services(vec![
            "Last.FM".to_string(),
            "youtube-music".to_string(),
            "ddg".to_string(),
            "odesli".to_string(),
            "rutracker".to_string(),
            "yandex-video".to_string(),
        ])
        .expect("parse services");

        assert!(services.contains(&ExternalLinkService::LastFm));
        assert!(services.contains(&ExternalLinkService::YouTubeMusic));
        assert!(services.contains(&ExternalLinkService::DuckDuckGo));
        assert!(services.contains(&ExternalLinkService::Songlink));
        assert!(services.contains(&ExternalLinkService::RuTracker));
        assert!(services.contains(&ExternalLinkService::YandexVideo));
    }

    #[test]
    fn rejects_unknown_external_link_service() {
        let error =
            parse_external_link_services(vec!["unknown".to_string()]).expect_err("invalid service");

        assert!(error.to_string().contains("unknown external link service"));
        assert!(error.to_string().contains("spotify"));
    }

    #[test]
    fn filters_related_links_with_whitelist_and_blocklist() {
        let track = sample_track();
        let enabled_services =
            parse_external_link_services(vec!["spotify".to_string(), "google".to_string()])
                .expect("enabled services");
        let disabled_services =
            parse_external_link_services(vec!["google".to_string()]).expect("disabled services");

        assert_eq!(
            related_music_service_links(&track, &enabled_services, &disabled_services),
            vec![ExternalLink::new(
                "Spotify search",
                "https://open.spotify.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
            )]
        );

        let enabled_services =
            parse_external_link_services(vec!["youtube".to_string(), "youtube-music".to_string()])
                .expect("enabled YouTube services");
        let disabled_services =
            parse_external_link_services(vec!["youtube".to_string()]).expect("disabled YouTube");

        assert_eq!(
            youtube_links(&track, &enabled_services, &disabled_services),
            vec![ExternalLink::new(
                "YouTube Music search",
                "https://music.youtube.com/search?q=Artist+Name+Song+%26+Name+Album+Name"
            )]
        );
    }

    #[test]
    fn builds_fallback_links_without_artist_or_album() {
        let mut track = sample_track();
        track.artists.clear();
        track.albums.clear();
        track.duration_ms = None;
        let enabled_services = no_enabled_services();
        let disabled_services = no_disabled_services();

        assert_eq!(musicbrainz_query(&track), "recording:\"Song & Name\"");
        assert_eq!(human_query(&track), "Song & Name");
        assert_eq!(
            lrclib_get_url(&track),
            "https://lrclib.net/api/get?track_name=Song+%26+Name&artist_name=&album_name="
        );
        assert_eq!(
            wikimedia_links(&track, &enabled_services, &disabled_services, true),
            vec![
                ExternalLink::new(
                    "Wikidata track search",
                    "https://www.wikidata.org/w/index.php?search=Song+%26+Name"
                ),
                ExternalLink::new(
                    "Wikipedia track search",
                    "https://en.wikipedia.org/w/index.php?search=Song+%26+Name"
                ),
            ]
        );
        assert_eq!(musicbrainz_entity_search_links(&track), Vec::new());
        assert_eq!(
            related_music_service_links(&track, &enabled_services, &disabled_services)
                .first()
                .expect("spotify link"),
            &ExternalLink::new(
                "Spotify search",
                "https://open.spotify.com/search?q=Song+%26+Name"
            )
        );
        assert!(lastfm_links(&track).contains(&ExternalLink::new(
            "Last.fm track search",
            "https://www.last.fm/search/tracks?q=Song+%26+Name"
        )));
        assert!(
            related_music_service_links(&track, &enabled_services, &disabled_services).contains(
                &ExternalLink::new(
                    "Yandex Video track search",
                    "https://yandex.ru/video/search?text=Song+%26+Name"
                )
            )
        );
    }

    #[test]
    fn builds_wikidata_track_search_with_artist_context() {
        let track = sample_track();
        let enabled_services = no_enabled_services();
        let disabled_services = no_disabled_services();

        assert_eq!(
            wikimedia_links(&track, &enabled_services, &disabled_services, true),
            vec![
                ExternalLink::new(
                    "Wikidata track search",
                    "https://www.wikidata.org/w/index.php?search=Artist+Name+Song+%26+Name"
                ),
                ExternalLink::new(
                    "Wikipedia track search",
                    "https://en.wikipedia.org/w/index.php?search=Artist+Name+Song+%26+Name"
                ),
                ExternalLink::new(
                    "Wikidata artist search",
                    "https://www.wikidata.org/w/index.php?search=Artist+Name"
                ),
                ExternalLink::new(
                    "Wikipedia artist search",
                    "https://en.wikipedia.org/w/index.php?search=Artist+Name"
                ),
                ExternalLink::new(
                    "Wikidata album search",
                    "https://www.wikidata.org/w/index.php?search=Artist+Name+Album+Name"
                ),
                ExternalLink::new(
                    "Wikipedia album search",
                    "https://en.wikipedia.org/w/index.php?search=Artist+Name+Album+Name"
                ),
            ]
        );
    }

    #[test]
    fn can_skip_wikidata_track_search_after_exact_item_match() {
        let track = sample_track();
        let enabled_services = no_enabled_services();
        let disabled_services = no_disabled_services();

        let links = wikimedia_links(&track, &enabled_services, &disabled_services, false);

        assert!(
            !links
                .iter()
                .any(|link| link.label == "Wikidata track search")
        );
        assert!(links.contains(&ExternalLink::new(
            "Wikidata artist search",
            "https://www.wikidata.org/w/index.php?search=Artist+Name"
        )));
        assert!(links.contains(&ExternalLink::new(
            "Wikipedia track search",
            "https://en.wikipedia.org/w/index.php?search=Artist+Name+Song+%26+Name"
        )));
    }

    #[test]
    fn builds_exact_wikidata_lookup_urls() {
        let lookup_url = wikidata_track_item_lookup_url("105978624").expect("valid lookup URL");
        assert_eq!(
            lookup_url,
            "https://www.wikidata.org/w/api.php?format=json&formatversion=2&action=query&list=search&srsearch=haswbstatement%3AP13289%3D105978624&srnamespace=0&srlimit=1&srprop="
        );
        assert!(wikidata_track_item_lookup_url("not-a-number").is_none());

        let claims_url = wikidata_track_claims_lookup_url("Q105978624").expect("valid claims URL");
        assert_eq!(
            claims_url,
            "https://www.wikidata.org/w/api.php?format=json&formatversion=2&action=wbgetentities&ids=Q105978624&props=claims"
        );
        assert!(wikidata_track_claims_lookup_url("P13289").is_none());
    }

    #[test]
    fn extracts_wikidata_item_id_from_search_response() {
        let response = serde_json::from_str::<WikidataSearchResponse>(
            r#"{
                "query": {
                    "search": [
                        {"title": "Project:Ignore"},
                        {"title": "Q105978624"}
                    ]
                }
            }"#,
        )
        .expect("search response");

        assert_eq!(
            wikidata_track_item_id(&response),
            Some("Q105978624".to_string())
        );
    }

    #[test]
    fn converts_wikidata_claims_into_exact_external_links() {
        let response = serde_json::from_str::<WikidataEntitiesResponse>(
            r#"{
                "entities": {
                    "Q105978624": {
                        "claims": {
                            "P921": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": {"entity-type": "item", "numeric-id": 2, "id": "Q2"}}},
                                "rank": "normal"
                            }],
                            "P737": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": {"entity-type": "item", "numeric-id": 3, "id": "Q3"}}},
                                "rank": "normal"
                            }],
                            "P407": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": {"entity-type": "item", "numeric-id": 4, "id": "Q4"}}},
                                "rank": "normal"
                            }],
                            "P4404": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}},
                                "rank": "normal"
                            }],
                            "P1651": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": "dQw4w9WgXcQ"}},
                                "rank": "normal"
                            }],
                            "P2207": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": "4uLU6hMCjMI75M1A2tKUQC"}},
                                "rank": "normal"
                            }],
                            "P10110": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": "1440833098?i=1440833918"}},
                                "rank": "normal"
                            }],
                            "P2724": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": "3135556"}},
                                "rank": "normal"
                            }],
                            "P3040": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": "artist/song"}},
                                "rank": "normal"
                            }],
                            "P4578": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": "12345"}},
                                "rank": "normal"
                            }],
                            "P1730": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": "mt0000000000"}},
                                "rank": "normal"
                            }],
                            "P6079": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"}},
                                "rank": "normal"
                            }],
                            "P6218": [{
                                "mainsnak": {"snaktype": "value", "datavalue": {"value": "Artist-song-lyrics"}},
                                "rank": "normal"
                            }]
                        }
                    }
                }
            }"#,
        )
        .expect("entities response");
        let claims = &response.entities["Q105978624"].claims;
        let labels = HashMap::from([
            ("Q2".to_string(), "Main topic".to_string()),
            ("Q3".to_string(), "Influence".to_string()),
            ("Q4".to_string(), "English".to_string()),
        ]);
        let enabled_services = no_enabled_services();
        let disabled_services = no_disabled_services();

        assert_eq!(
            wikidata_related_item_ids(claims),
            vec!["Q2".to_string(), "Q3".to_string(), "Q4".to_string()]
        );
        assert_eq!(
            wikidata_links_from_claims(claims, &labels, &enabled_services, &disabled_services),
            vec![
                ExternalLink::new("Wikidata main subject: Main topic", wikidata_item_url("Q2")),
                ExternalLink::new("Wikidata influenced by: Influence", wikidata_item_url("Q3")),
                ExternalLink::new("Wikidata language: English", wikidata_item_url("Q4")),
                ExternalLink::new(
                    "MusicBrainz recording",
                    "https://musicbrainz.org/recording/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
                ),
                ExternalLink::new(
                    "ListenBrainz recording metadata",
                    "https://api.listenbrainz.org/1/metadata/recording/?recording_mbids=aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa&inc=artist+release"
                ),
                ExternalLink::new(
                    "TheAudioDB track MBID lookup",
                    "https://www.theaudiodb.com/api/v1/json/2/track-mb.php?i=aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
                ),
                ExternalLink::youtube_embed("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
                ExternalLink::new(
                    "Spotify track",
                    "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"
                ),
                ExternalLink::new(
                    "Apple Music track",
                    "https://geo.music.apple.com/album/id1440833098?i=1440833918"
                ),
                ExternalLink::new("Deezer track", "https://www.deezer.com/track/3135556"),
                ExternalLink::new("SoundCloud", "https://soundcloud.com/artist/song"),
                ExternalLink::new("TIDAL track", "https://tidal.com/browse/track/12345"),
                ExternalLink::new(
                    "AllMusic song",
                    "https://www.allmusic.com/song/mt0000000000"
                ),
                ExternalLink::new(
                    "Discogs track",
                    "https://www.discogs.com/track/bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
                ),
                ExternalLink::new("Genius", "https://genius.com/Artist-song-lyrics"),
            ]
        );
    }

    #[test]
    fn uses_songlink_retry_after_seconds() {
        let retry_after = HeaderValue::from_static("42");

        assert_eq!(
            songlink_retry_delay(Some(&retry_after), 0),
            Duration::from_secs(42)
        );
    }

    #[test]
    fn caps_songlink_retry_after_seconds() {
        let retry_after = HeaderValue::from_static("3600");

        assert_eq!(
            songlink_retry_delay(Some(&retry_after), 0),
            SONGLINK_MAX_RETRY_AFTER
        );
    }

    #[test]
    fn falls_back_to_songlink_backoff_without_valid_retry_after() {
        let retry_after = HeaderValue::from_static("not-a-number");

        assert_eq!(
            songlink_retry_delay(Some(&retry_after), 1),
            Duration::from_secs(30)
        );
        assert_eq!(songlink_retry_delay(None, 99), Duration::from_secs(30));
    }

    #[test]
    fn escapes_musicbrainz_lucene_query_parts() {
        let mut track = sample_track();
        track.title = "Quote \" and slash \\".to_string();
        track.artists = vec!["Artist \"Name\"".to_string()];

        assert_eq!(
            musicbrainz_query(&track),
            "recording:\"Quote \\\" and slash \\\\\" AND artist:\"Artist \\\"Name\\\"\""
        );
    }

    #[test]
    fn decodes_non_numeric_musicbrainz_score_as_zero() {
        let recording =
            serde_json::from_str::<MusicBrainzRecording>(r#"{"id":"id","score":"not-a-number"}"#)
                .expect("deserialize recording");

        assert_eq!(recording.score, 0);
        assert_eq!(recording.length, None);
    }
}

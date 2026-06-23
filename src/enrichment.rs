use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use reqwest::{
    StatusCode,
    header::{HeaderValue, RETRY_AFTER},
};
use serde::{Deserialize, Deserializer};
use tokio::time::sleep;
use tracing::warn;
use url::form_urlencoded;

use crate::yandex::LikedTrack;

const CLIENT_NAME: &str = concat!("yandex-music-likes-to-evernote/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const MUSICBRAINZ_MIN_SCORE: i64 = 90;
const MUSICBRAINZ_DURATION_TOLERANCE_MS: u128 = 7_000;
const SONGLINK_MAX_ATTEMPTS: usize = 3;
const SONGLINK_BACKOFFS: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];
const SONGLINK_MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalLink {
    pub label: String,
    pub url: String,
}

impl ExternalLink {
    fn new(label: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            url: url.into(),
        }
    }
}

#[derive(Clone)]
pub struct EnrichmentClient {
    http: reqwest::Client,
    genius_access_token: Option<String>,
    songlink_user_country: String,
    // Once Songlink keeps returning 429 after retries, avoid slowing every later
    // track in the same sync run with more calls that are likely to fail.
    songlink_rate_limited: Arc<AtomicBool>,
}

impl EnrichmentClient {
    pub fn new(
        genius_access_token: Option<String>,
        songlink_user_country: impl Into<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(CLIENT_NAME)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build enrichment HTTP client")?;
        Ok(Self {
            http,
            genius_access_token,
            songlink_user_country: songlink_user_country.into(),
            songlink_rate_limited: Arc::new(AtomicBool::new(false)),
        })
    }

    pub async fn links_for(&self, track: &LikedTrack) -> Vec<ExternalLink> {
        let mut links = Vec::new();

        links.push(self.musicbrainz_link(track).await.unwrap_or_else(|| {
            ExternalLink::new(
                "MusicBrainz recording search",
                musicbrainz_search_url(track),
            )
        }));

        links.push(self.lrclib_link(track).await.unwrap_or_else(|| {
            ExternalLink::new("LRCLIB lyrics search", lrclib_search_url(track))
        }));

        links.push(self.songlink_link(track).await.unwrap_or_else(|| {
            ExternalLink::new("Songlink/Odesli lookup", songlink_lookup_url(track))
        }));

        links.extend(wikimedia_links(track));
        links.push(ExternalLink::new(
            "YouTube search",
            youtube_search_url(track),
        ));
        links.push(
            self.genius_link(track)
                .await
                .unwrap_or_else(|| ExternalLink::new("Genius search", genius_search_url(track))),
        );

        links
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

fn wikimedia_links(track: &LikedTrack) -> Vec<ExternalLink> {
    let mut links = Vec::new();

    let track_query = if let Some(artist) = track
        .artists
        .first()
        .filter(|artist| !artist.trim().is_empty())
    {
        format!("{artist} {}", track.title)
    } else {
        track.title.clone()
    };
    if !track_query.trim().is_empty() {
        links.push(ExternalLink::new(
            "Wikidata track search",
            wikidata_search_url(&track_query),
        ));
    }

    if let Some(artist) = track
        .artists
        .first()
        .filter(|artist| !artist.trim().is_empty())
    {
        links.push(ExternalLink::new(
            "Wikidata artist search",
            wikidata_search_url(artist),
        ));
        links.push(ExternalLink::new(
            "Wikipedia artist search",
            wikipedia_search_url(artist),
        ));
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
        links.push(ExternalLink::new(
            "Wikidata album search",
            wikidata_search_url(&query),
        ));
        links.push(ExternalLink::new(
            "Wikipedia album search",
            wikipedia_search_url(&query),
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

fn wikidata_search_url(query: &str) -> String {
    query_url("https://www.wikidata.org/w/index.php", &[("search", query)])
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

fn youtube_search_url(track: &LikedTrack) -> String {
    query_url(
        "https://www.youtube.com/results",
        &[("search_query", &human_query(track))],
    )
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
            duration_ms: Some(123_000),
            cover_url: None,
            yandex_url: "https://music.yandex.com/track/123".to_string(),
        }
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
    fn builds_fallback_links_without_artist_or_album() {
        let mut track = sample_track();
        track.artists.clear();
        track.albums.clear();
        track.duration_ms = None;

        assert_eq!(musicbrainz_query(&track), "recording:\"Song & Name\"");
        assert_eq!(human_query(&track), "Song & Name");
        assert_eq!(
            lrclib_get_url(&track),
            "https://lrclib.net/api/get?track_name=Song+%26+Name&artist_name=&album_name="
        );
        assert_eq!(
            wikimedia_links(&track),
            vec![ExternalLink::new(
                "Wikidata track search",
                "https://www.wikidata.org/w/index.php?search=Song+%26+Name"
            )]
        );
    }

    #[test]
    fn builds_wikidata_track_search_with_artist_context() {
        let track = sample_track();

        assert!(wikimedia_links(&track).contains(&ExternalLink::new(
            "Wikidata track search",
            "https://www.wikidata.org/w/index.php?search=Artist+Name+Song+%26+Name"
        )));
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

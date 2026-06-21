use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use yandex_music::{
    API_PATH, YandexMusicClient, api::track::get_liked_tracks::GetLikedTracksOptions,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LikedTrack {
    pub id: String,
    pub liked_at: DateTime<Utc>,
    pub title: String,
    pub artists: Vec<String>,
    pub albums: Vec<String>,
    pub duration_ms: Option<u128>,
    pub cover_url: Option<String>,
    pub yandex_url: String,
}

pub struct YandexClient {
    inner: YandexMusicClient,
}

impl YandexClient {
    pub fn new(token: &str) -> Result<Self> {
        let inner = YandexMusicClient::builder(token)
            .build()
            .context("failed to build Yandex Music client")?;
        Ok(Self { inner })
    }

    pub async fn liked_tracks(&self) -> Result<Vec<LikedTrack>> {
        let status = self
            .inner
            .get_account_status()
            .await
            .context("failed to fetch Yandex Music account status")?;
        let user_id = status
            .account
            .uid
            .ok_or_else(|| anyhow!("Yandex Music account status did not include a user id"))?;

        let library = self
            .inner
            .get_liked_tracks(&GetLikedTracksOptions::new(user_id))
            .await
            .context("failed to fetch Yandex Music liked tracks")?;

        if library.tracks.is_empty() {
            return Ok(Vec::new());
        }

        let liked_at_by_track_id = library
            .tracks
            .iter()
            .map(|track| (track.id.clone(), track.timestamp))
            .collect::<HashMap<_, _>>();
        let ids = library
            .tracks
            .iter()
            .map(|track| track.id.clone())
            .collect::<Vec<_>>();

        let mut tracks = Vec::new();
        for chunk in ids.chunks(100) {
            let rich_tracks = self.fetch_tracks(chunk).await.with_context(|| {
                format!(
                    "failed to fetch Yandex Music track details for {} tracks",
                    chunk.len()
                )
            })?;

            tracks.extend(rich_tracks.into_iter().filter_map(|track| {
                let liked_at = liked_at_by_track_id.get(&track.id).copied()?;
                Some(to_liked_track(track, liked_at))
            }));
        }

        tracks.sort_by_key(|track| track.liked_at);
        Ok(tracks)
    }

    async fn fetch_tracks(&self, track_ids: &[String]) -> Result<Vec<RawTrack>> {
        let track_ids = track_ids.join(",") + ",";
        let response = self
            .inner
            .inner
            .post(format!("{API_PATH}tracks"))
            .form(&[
                ("track-ids", track_ids),
                ("with-positions", "false".to_string()),
            ])
            .send()
            .await
            .context("failed to request Yandex Music track details")?
            .error_for_status()
            .context("Yandex Music returned an HTTP error for track details")?;
        let response = response
            .json::<YandexApiResponse>()
            .await
            .context("failed to parse Yandex Music track details response")?;

        if let Some(error) = response.error {
            return Err(anyhow!(
                "Yandex Music API error while fetching track details: {}{}",
                error.name,
                error
                    .message
                    .map(|message| format!(": {message}"))
                    .unwrap_or_default()
            ));
        }

        let result = response
            .result
            .context("Yandex Music track details response did not include result")?;
        let tracks = serde_json::from_value::<Vec<Option<RawTrack>>>(result)
            .context("failed to decode Yandex Music track details")?
            .into_iter()
            .flatten()
            .collect();

        Ok(tracks)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YandexApiResponse {
    result: Option<Value>,
    error: Option<YandexApiError>,
}

#[derive(Debug, Deserialize)]
struct YandexApiError {
    name: String,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTrack {
    #[serde(deserialize_with = "number_or_string_to_string")]
    id: String,
    title: Option<String>,
    #[serde(default)]
    artists: Vec<RawArtist>,
    #[serde(default)]
    albums: Vec<RawAlbum>,
    cover_uri: Option<String>,
    og_image: Option<String>,
    #[serde(
        default,
        rename = "durationMs",
        deserialize_with = "optional_u128_from_number_or_string"
    )]
    duration_ms: Option<u128>,
}

#[derive(Debug, Deserialize)]
struct RawArtist {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawAlbum {
    title: Option<String>,
}

fn to_liked_track(track: RawTrack, liked_at: DateTime<Utc>) -> LikedTrack {
    let artists = track.artists.iter().filter_map(artist_name).collect();
    let albums = track.albums.iter().filter_map(album_title).collect();
    let cover_url = track
        .cover_uri
        .or_else(|| track.og_image.clone())
        .map(normalize_cover_url);
    let duration_ms = track.duration_ms;
    let yandex_url = format!("https://music.yandex.com/track/{}", track.id);
    let title = track.title.unwrap_or_else(|| format!("Track {}", track.id));

    LikedTrack {
        id: track.id,
        liked_at,
        title,
        artists,
        albums,
        duration_ms,
        cover_url,
        yandex_url,
    }
}

fn artist_name(artist: &RawArtist) -> Option<String> {
    artist
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn album_title(album: &RawAlbum) -> Option<String> {
    album
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_cover_url(uri: String) -> String {
    let uri = uri.replace("%%", "400x400");
    if uri.starts_with("http://") || uri.starts_with("https://") {
        uri
    } else {
        format!("https://{uri}")
    }
}

fn number_or_string_to_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::String(value) => Ok(value),
        Value::Number(value) => Ok(value.to_string()),
        value => Err(serde::de::Error::custom(format!(
            "expected string or number, got {value}"
        ))),
    }
}

fn optional_u128_from_number_or_string<'de, D>(deserializer: D) -> Result<Option<u128>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .map(|value| Some(value as u128))
            .ok_or_else(|| serde::de::Error::custom("expected unsigned integer")),
        Some(Value::String(value)) if value.trim().is_empty() => Ok(None),
        Some(Value::String(value)) => value
            .parse::<u128>()
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(value) => Err(serde::de::Error::custom(format!(
            "expected string, number, or null, got {value}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_cover_uri() {
        assert_eq!(
            normalize_cover_url("avatars.yandex.net/get-music-content/1/abc%%".to_string()),
            "https://avatars.yandex.net/get-music-content/1/abc400x400"
        );
    }

    #[test]
    fn decodes_track_details_with_unexpected_metadata_values() {
        let track = serde_json::from_str::<RawTrack>(
            r#"{
                "id": 123,
                "title": "Track Title",
                "artists": [{"name": "Artist"}],
                "albums": [{"title": "Album"}],
                "coverUri": "avatars.yandex.net/get-music-content/1/abc%%",
                "durationMs": 123000,
                "metaData": {
                    "volume": 2019
                }
            }"#,
        )
        .expect("track should decode despite irrelevant bad metadata");

        let liked_at = DateTime::parse_from_rfc3339("2026-06-20T00:00:00Z")
            .expect("valid date")
            .with_timezone(&Utc);
        let track = to_liked_track(track, liked_at);

        assert_eq!(track.id, "123");
        assert_eq!(track.title, "Track Title");
        assert_eq!(track.artists, vec!["Artist"]);
        assert_eq!(track.albums, vec!["Album"]);
        assert_eq!(track.duration_ms, Some(123000));
    }

    #[test]
    fn decodes_string_ids_and_duration_values() {
        let track = serde_json::from_str::<RawTrack>(
            r#"{
                "id": "track-id",
                "title": null,
                "artists": [{"name": "  "}, {"name": "Artist"}],
                "albums": [{"title": ""}, {"title": "Album"}],
                "ogImage": "https://example.test/cover.jpg",
                "durationMs": "45000"
            }"#,
        )
        .expect("track should decode");

        let liked_at = DateTime::parse_from_rfc3339("2026-06-20T00:00:00Z")
            .expect("valid date")
            .with_timezone(&Utc);
        let track = to_liked_track(track, liked_at);

        assert_eq!(track.id, "track-id");
        assert_eq!(track.title, "Track track-id");
        assert_eq!(track.artists, vec!["Artist"]);
        assert_eq!(track.albums, vec!["Album"]);
        assert_eq!(
            track.cover_url.as_deref(),
            Some("https://example.test/cover.jpg")
        );
        assert_eq!(track.duration_ms, Some(45_000));
    }

    #[test]
    fn rejects_invalid_track_id_type() {
        let error = serde_json::from_str::<RawTrack>(
            r#"{
                "id": {"unexpected": true}
            }"#,
        )
        .expect_err("invalid id should fail");

        assert!(error.to_string().contains("expected string or number"));
    }
}

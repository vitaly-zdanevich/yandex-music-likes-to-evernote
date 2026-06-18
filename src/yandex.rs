use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use yandex_music::{
    YandexMusicClient,
    api::track::{get_liked_tracks::GetLikedTracksOptions, get_tracks::GetTracksOptions},
    model::{album::Album, artist::Artist, track::Track},
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
            let rich_tracks = self
                .inner
                .get_tracks(&GetTracksOptions::new(chunk.to_vec()))
                .await
                .with_context(|| {
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
}

fn to_liked_track(track: Track, liked_at: DateTime<Utc>) -> LikedTrack {
    let artists = track.artists.iter().filter_map(artist_name).collect();
    let albums = track.albums.iter().filter_map(album_title).collect();
    let cover_url = track
        .cover_uri
        .or_else(|| track.og_image.clone())
        .map(normalize_cover_url);
    let duration_ms = track.duration.map(|duration| duration.as_millis());
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

fn artist_name(artist: &Artist) -> Option<String> {
    artist
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn album_title(album: &Album) -> Option<String> {
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
}

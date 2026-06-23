use anyhow::{Context, Result};
use chrono::Utc;
use tracing::{info, warn};

use crate::audio::{AudioAttachment, CoverAttachment, CoverImage, TrackAudio};
use crate::config::Settings;
use crate::enrichment::EnrichmentClient;
use crate::evernote_client::{EvernoteClient, ThriftHttpClient};
use crate::note;
use crate::state::State;
use crate::yandex::YandexClient;

pub fn run(settings: Settings) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("failed to create Tokio runtime")?;
    let yandex = YandexClient::new(&settings.yandex_music_token)?;
    let evernote = EvernoteClient::new(
        settings.evernote_auth_token.clone(),
        settings.evernote_note_store_url.clone(),
        settings.evernote_user_store_url.clone(),
        settings.evernote_notebook_guid.clone(),
        settings.evernote_tag_names()?,
    )?;
    let enrichment = if settings.enrich_external_links {
        Some(EnrichmentClient::new(
            settings.genius_access_token.clone(),
            settings.songlink_user_country.clone(),
            settings.acoustid_api_key.clone(),
            settings.enabled_external_link_service_names(),
            settings.disabled_external_link_service_names(),
        )?)
    } else {
        None
    };

    run_with_clients(&settings, &runtime, &yandex, &evernote, enrichment.as_ref())
}

// Private interfaces keep the sync state machine testable without reaching
// live Yandex Music or Evernote services.
trait MusicSource {
    fn liked_tracks(
        &self,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Vec<crate::yandex::LikedTrack>>;

    fn download_audio(
        &self,
        runtime: &tokio::runtime::Runtime,
        track_id: &str,
    ) -> Result<Option<TrackAudio>>;

    fn download_cover(
        &self,
        runtime: &tokio::runtime::Runtime,
        cover_url: &str,
    ) -> Result<Option<CoverImage>>;
}

impl MusicSource for YandexClient {
    fn liked_tracks(
        &self,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Vec<crate::yandex::LikedTrack>> {
        runtime.block_on(self.liked_tracks())
    }

    fn download_audio(
        &self,
        runtime: &tokio::runtime::Runtime,
        track_id: &str,
    ) -> Result<Option<TrackAudio>> {
        runtime.block_on(self.download_audio(track_id))
    }

    fn download_cover(
        &self,
        runtime: &tokio::runtime::Runtime,
        cover_url: &str,
    ) -> Result<Option<CoverImage>> {
        runtime.block_on(self.download_cover(cover_url))
    }
}

trait NoteSink {
    fn create_track_note(
        &self,
        title: String,
        content: String,
        source_url: String,
        cover: Option<&CoverAttachment>,
        audio: Option<&AudioAttachment>,
    ) -> Result<String>;
}

impl<C> NoteSink for EvernoteClient<C>
where
    C: ThriftHttpClient,
{
    fn create_track_note(
        &self,
        title: String,
        content: String,
        source_url: String,
        cover: Option<&CoverAttachment>,
        audio: Option<&AudioAttachment>,
    ) -> Result<String> {
        self.create_track_note(title, content, source_url, cover, audio)
    }
}

fn run_with_clients(
    settings: &Settings,
    runtime: &tokio::runtime::Runtime,
    yandex: &impl MusicSource,
    evernote: &impl NoteSink,
    enrichment: Option<&EnrichmentClient>,
) -> Result<()> {
    let mut state = State::load(&settings.state_path)?;
    let liked_tracks = yandex.liked_tracks(runtime)?;
    let mut new_tracks = liked_tracks
        .into_iter()
        .filter(|track| !state.contains(&track.id))
        .collect::<Vec<_>>();

    if settings.max_tracks_per_run != 0 && new_tracks.len() > settings.max_tracks_per_run {
        warn!(
            total_new_tracks = new_tracks.len(),
            limit = settings.max_tracks_per_run,
            "limiting tracks exported in this run"
        );
        new_tracks.truncate(settings.max_tracks_per_run);
    }

    info!(new_tracks = new_tracks.len(), "found new liked tracks");

    let mut saved_count = 0usize;
    for track in new_tracks {
        let title = note::title(&track);
        let downloaded_audio = if settings.backup_audio && !settings.dry_run {
            match yandex.download_audio(runtime, &track.id) {
                Ok(audio) => audio,
                Err(error) => {
                    // Transient failure (e.g. a flaky download host): leave the
                    // track unprocessed so the next run retries it with audio
                    // rather than creating a permanent audio-less note.
                    warn!(
                        track_id = track.id,
                        error = format!("{error:#}"),
                        "audio download failed; leaving track for the next run"
                    );
                    continue;
                }
            }
        } else {
            None
        };

        let external_links = if let Some(enrichment) = &enrichment {
            runtime.block_on(enrichment.links_for(&track, downloaded_audio.as_ref()))
        } else {
            Vec::new()
        };

        if settings.dry_run {
            info!(
                track_id = track.id,
                title = title,
                url = track.yandex_url,
                backup_audio = settings.backup_audio,
                "dry-run: would create Evernote note"
            );
            continue;
        }

        let audio = downloaded_audio.map(|audio| AudioAttachment::new(audio, &title));
        let cover = if let Some(cover_url) = track.cover_url.as_deref() {
            match yandex.download_cover(runtime, cover_url) {
                Ok(Some(cover)) => Some(CoverAttachment::new(cover)),
                Ok(None) => None,
                Err(error) => {
                    warn!(
                        track_id = track.id,
                        error = format!("{error:#}"),
                        "cover download failed; creating note without embedded cover"
                    );
                    None
                }
            }
        } else {
            None
        };

        let content = note::enml(&track, &external_links, cover.as_ref(), audio.as_ref());
        let guid = evernote.create_track_note(
            title.clone(),
            content,
            track.yandex_url.clone(),
            cover.as_ref(),
            audio.as_ref(),
        )?;
        info!(
            track_id = track.id,
            evernote_guid = guid,
            title = title,
            cover_attached = cover.is_some(),
            audio_attached = audio.is_some(),
            "created Evernote note"
        );
        saved_count += 1;
        info!("{saved_count} saved");

        state.mark_processed(track.id);
        state.save(&settings.state_path)?;
    }

    if !settings.dry_run {
        state.last_successful_sync_at = Some(Utc::now());
        state.save(&settings.state_path)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use anyhow::anyhow;
    use chrono::TimeZone;

    use super::*;
    use crate::yandex::LikedTrack;

    struct MockMusicSource {
        tracks: Vec<LikedTrack>,
    }

    impl MusicSource for MockMusicSource {
        fn liked_tracks(&self, _runtime: &tokio::runtime::Runtime) -> Result<Vec<LikedTrack>> {
            Ok(self.tracks.clone())
        }

        fn download_audio(
            &self,
            _runtime: &tokio::runtime::Runtime,
            _track_id: &str,
        ) -> Result<Option<TrackAudio>> {
            Ok(None)
        }

        fn download_cover(
            &self,
            _runtime: &tokio::runtime::Runtime,
            _cover_url: &str,
        ) -> Result<Option<CoverImage>> {
            Ok(None)
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct CreatedNote {
        title: String,
        source_url: String,
    }

    #[derive(Default)]
    struct MockNoteSink {
        results: Mutex<VecDeque<Result<String, String>>>,
        created_notes: Mutex<Vec<CreatedNote>>,
    }

    impl MockNoteSink {
        fn push_success(&self, guid: impl Into<String>) {
            self.results
                .lock()
                .expect("results lock")
                .push_back(Ok(guid.into()));
        }

        fn push_error(&self, error: impl Into<String>) {
            self.results
                .lock()
                .expect("results lock")
                .push_back(Err(error.into()));
        }

        fn created_notes(&self) -> Vec<CreatedNote> {
            self.created_notes
                .lock()
                .expect("created notes lock")
                .iter()
                .map(|note| CreatedNote {
                    title: note.title.clone(),
                    source_url: note.source_url.clone(),
                })
                .collect()
        }
    }

    impl NoteSink for MockNoteSink {
        fn create_track_note(
            &self,
            title: String,
            _content: String,
            source_url: String,
            _cover: Option<&CoverAttachment>,
            _audio: Option<&AudioAttachment>,
        ) -> Result<String> {
            self.created_notes
                .lock()
                .expect("created notes lock")
                .push(CreatedNote { title, source_url });
            match self
                .results
                .lock()
                .expect("results lock")
                .pop_front()
                .expect("queued note result")
            {
                Ok(guid) => Ok(guid),
                Err(error) => Err(anyhow!(error)),
            }
        }
    }

    fn test_settings(state_path: PathBuf) -> Settings {
        Settings {
            yandex_music_token: "yandex-token".to_string(),
            evernote_auth_token: "evernote-token".to_string(),
            evernote_note_store_url: Some("https://example.test/notestore".to_string()),
            evernote_user_store_url: "https://example.test/user".to_string(),
            evernote_notebook_guid: None,
            evernote_tags: "yandex-music".to_string(),
            state_path,
            dry_run: false,
            max_tracks_per_run: 0,
            backup_audio: false,
            enrich_external_links: false,
            genius_access_token: None,
            acoustid_api_key: None,
            enabled_external_link_services: String::new(),
            disabled_external_link_services: String::new(),
            songlink_user_country: "US".to_string(),
        }
    }

    fn liked_track(id: &str, title: &str) -> LikedTrack {
        LikedTrack {
            id: id.to_string(),
            title: title.to_string(),
            artists: vec!["Artist".to_string()],
            artist_links: Vec::new(),
            albums: Vec::new(),
            album_links: Vec::new(),
            duration_ms: None,
            cover_url: None,
            yandex_url: format!("https://music.yandex.com/track/{id}"),
            liked_at: chrono::Utc
                .with_ymd_and_hms(2026, 6, 23, 12, 0, 0)
                .single()
                .expect("valid liked_at"),
        }
    }

    #[test]
    fn failed_note_creation_does_not_mark_track_processed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = test_settings(dir.path().join("state.json"));
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let yandex = MockMusicSource {
            tracks: vec![liked_track("track-1", "Track One")],
        };
        let evernote = MockNoteSink::default();
        evernote.push_error("Evernote transport failed");

        let error = run_with_clients(&settings, &runtime, &yandex, &evernote, None)
            .expect_err("note creation should fail");
        let state = State::load(&settings.state_path).expect("state");

        assert_eq!(error.to_string(), "Evernote transport failed");
        assert!(!state.contains("track-1"));
        assert_eq!(
            evernote.created_notes(),
            vec![CreatedNote {
                title: "Artist - Track One".to_string(),
                source_url: "https://music.yandex.com/track/track-1".to_string(),
            }]
        );
    }

    #[test]
    fn successful_note_creation_saves_state_before_later_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = test_settings(dir.path().join("state.json"));
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let yandex = MockMusicSource {
            tracks: vec![
                liked_track("track-1", "Track One"),
                liked_track("track-2", "Track Two"),
            ],
        };
        let evernote = MockNoteSink::default();
        evernote.push_success("guid-1");
        evernote.push_error("Evernote transport failed");

        run_with_clients(&settings, &runtime, &yandex, &evernote, None)
            .expect_err("second note should fail");
        let state = State::load(&settings.state_path).expect("state");

        assert!(state.contains("track-1"));
        assert!(!state.contains("track-2"));
    }

    #[test]
    fn next_run_skips_processed_ids_and_exports_remaining_likes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = test_settings(dir.path().join("state.json"));
        let mut state = State::default();
        state.mark_processed("track-1");
        state
            .save(&settings.state_path)
            .expect("save initial state");
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let yandex = MockMusicSource {
            tracks: vec![
                liked_track("track-1", "Track One"),
                liked_track("track-2", "Track Two"),
                liked_track("track-3", "Track Three"),
            ],
        };
        let evernote = MockNoteSink::default();
        evernote.push_success("guid-2");
        evernote.push_success("guid-3");

        run_with_clients(&settings, &runtime, &yandex, &evernote, None).expect("sync");
        let state = State::load(&settings.state_path).expect("state");

        assert!(state.contains("track-1"));
        assert!(state.contains("track-2"));
        assert!(state.contains("track-3"));
        assert!(state.last_successful_sync_at.is_some());
        assert_eq!(
            evernote.created_notes(),
            vec![
                CreatedNote {
                    title: "Artist - Track Two".to_string(),
                    source_url: "https://music.yandex.com/track/track-2".to_string(),
                },
                CreatedNote {
                    title: "Artist - Track Three".to_string(),
                    source_url: "https://music.yandex.com/track/track-3".to_string(),
                },
            ]
        );
    }
}

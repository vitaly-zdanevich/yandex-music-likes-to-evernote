use anyhow::{Context, Result};
use chrono::Utc;
use tracing::{info, warn};

use crate::audio::{AudioAttachment, CoverAttachment};
use crate::config::Settings;
use crate::enrichment::EnrichmentClient;
use crate::evernote_client::EvernoteClient;
use crate::note;
use crate::state::State;
use crate::yandex::YandexClient;

pub fn run(settings: Settings) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("failed to create Tokio runtime")?;
    let mut state = State::load(&settings.state_path)?;
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

    let liked_tracks = runtime.block_on(yandex.liked_tracks())?;
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
            match runtime.block_on(yandex.download_audio(&track.id)) {
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
            match runtime.block_on(yandex.download_cover(cover_url)) {
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

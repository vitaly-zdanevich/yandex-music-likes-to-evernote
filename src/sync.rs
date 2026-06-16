use anyhow::Result;
use chrono::Utc;
use tracing::{info, warn};

use crate::config::Settings;
use crate::enrichment::EnrichmentClient;
use crate::evernote_client::EvernoteClient;
use crate::note;
use crate::state::State;
use crate::yandex::YandexClient;

pub async fn run(settings: Settings) -> Result<()> {
    let mut state = State::load(&settings.state_path)?;
    let yandex = YandexClient::new(&settings.yandex_music_token)?;
    let evernote = EvernoteClient::new(
        settings.evernote_auth_token.clone(),
        settings.evernote_note_store_url.clone(),
        settings.evernote_notebook_guid.clone(),
    )?;
    let enrichment = if settings.enrich_external_links {
        Some(EnrichmentClient::new(
            settings.genius_access_token.clone(),
            settings.songlink_user_country.clone(),
        )?)
    } else {
        None
    };

    let liked_tracks = yandex.liked_tracks().await?;
    let mut new_tracks = liked_tracks
        .into_iter()
        .filter(|track| !state.contains(&track.id))
        .collect::<Vec<_>>();

    if new_tracks.len() > settings.max_tracks_per_run {
        warn!(
            total_new_tracks = new_tracks.len(),
            limit = settings.max_tracks_per_run,
            "limiting tracks exported in this run"
        );
        new_tracks.truncate(settings.max_tracks_per_run);
    }

    info!(new_tracks = new_tracks.len(), "found new liked tracks");

    for track in new_tracks {
        let title = note::title(&track);
        let external_links = if let Some(enrichment) = &enrichment {
            enrichment.links_for(&track).await
        } else {
            Vec::new()
        };
        let content = note::enml(&track, &external_links);

        if settings.dry_run {
            info!(
                track_id = track.id,
                title = title,
                url = track.yandex_url,
                "dry-run: would create Evernote note"
            );
            continue;
        } else {
            let guid = evernote.create_track_note(title.clone(), content)?;
            info!(
                track_id = track.id,
                evernote_guid = guid,
                title = title,
                "created Evernote note"
            );
        }

        state.mark_processed(track.id);
        state.save(&settings.state_path)?;
    }

    if !settings.dry_run {
        state.last_successful_sync_at = Some(Utc::now());
        state.save(&settings.state_path)?;
    }

    Ok(())
}

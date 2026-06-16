use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Parser;

#[derive(Debug, Clone, Parser)]
pub struct Settings {
    #[arg(long, env = "YANDEX_MUSIC_TOKEN")]
    pub yandex_music_token: String,
    #[arg(long, env = "EVERNOTE_AUTH_TOKEN")]
    pub evernote_auth_token: String,
    #[arg(long, env = "EVERNOTE_NOTE_STORE_URL")]
    pub evernote_note_store_url: String,
    #[arg(long, env = "EVERNOTE_NOTEBOOK_GUID")]
    pub evernote_notebook_guid: Option<String>,
    #[arg(long, env = "STATE_PATH", default_value = "state.json")]
    pub state_path: PathBuf,
    #[arg(long, env = "DRY_RUN", default_value_t = false)]
    pub dry_run: bool,
    #[arg(long, env = "MAX_TRACKS_PER_RUN", default_value_t = 10)]
    pub max_tracks_per_run: usize,
    #[arg(long, env = "ENRICH_EXTERNAL_LINKS", default_value_t = true)]
    pub enrich_external_links: bool,
    #[arg(long, env = "GENIUS_ACCESS_TOKEN")]
    pub genius_access_token: Option<String>,
    #[arg(long, env = "SONGLINK_USER_COUNTRY", default_value = "US")]
    pub songlink_user_country: String,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let settings = Self::parse_from(std::iter::once("yandex-music-likes-to-evernote"));
        settings.validate()
    }

    fn validate(mut self) -> Result<Self> {
        require_non_empty("YANDEX_MUSIC_TOKEN", &self.yandex_music_token)?;
        require_non_empty("EVERNOTE_AUTH_TOKEN", &self.evernote_auth_token)?;
        require_non_empty("EVERNOTE_NOTE_STORE_URL", &self.evernote_note_store_url)?;
        self.evernote_notebook_guid = self
            .evernote_notebook_guid
            .map(|guid| guid.trim().to_string())
            .filter(|guid| !guid.is_empty());
        self.genius_access_token = self
            .genius_access_token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty());
        self.songlink_user_country = self.songlink_user_country.trim().to_uppercase();

        if self.max_tracks_per_run == 0 {
            return Err(anyhow!("MAX_TRACKS_PER_RUN must be greater than 0"));
        }
        if self.songlink_user_country.len() != 2 {
            return Err(anyhow!(
                "SONGLINK_USER_COUNTRY must be a two-letter country code"
            ));
        }

        Ok(self)
    }
}

fn require_non_empty(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(anyhow!("{name} must not be empty"));
    }
    Ok(())
}

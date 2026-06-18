use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Parser;

pub const DEFAULT_EVERNOTE_USER_STORE_URL: &str = "https://www.evernote.com/edam/user";
pub const DEFAULT_EVERNOTE_TAG: &str = "yandex-music";

#[derive(Debug, Clone, Parser)]
pub struct Settings {
    #[arg(long, env = "YANDEX_MUSIC_TOKEN")]
    pub yandex_music_token: String,
    #[arg(long, env = "EVERNOTE_AUTH_TOKEN")]
    pub evernote_auth_token: String,
    #[arg(long, env = "EVERNOTE_NOTE_STORE_URL")]
    pub evernote_note_store_url: Option<String>,
    #[arg(
        long,
        env = "EVERNOTE_USER_STORE_URL",
        default_value = DEFAULT_EVERNOTE_USER_STORE_URL
    )]
    pub evernote_user_store_url: String,
    #[arg(long, env = "EVERNOTE_NOTEBOOK_GUID")]
    pub evernote_notebook_guid: Option<String>,
    #[arg(long, env = "EVERNOTE_TAGS", default_value = DEFAULT_EVERNOTE_TAG)]
    pub evernote_tags: String,
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
        self.evernote_note_store_url = self
            .evernote_note_store_url
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty());
        self.evernote_user_store_url = self.evernote_user_store_url.trim().to_string();
        require_non_empty("EVERNOTE_USER_STORE_URL", &self.evernote_user_store_url)?;
        self.evernote_notebook_guid = self
            .evernote_notebook_guid
            .map(|guid| guid.trim().to_string())
            .filter(|guid| !guid.is_empty());
        self.evernote_tags = parse_comma_separated("EVERNOTE_TAGS", &self.evernote_tags)?.join(",");
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

    pub fn evernote_tag_names(&self) -> Result<Vec<String>> {
        parse_comma_separated("EVERNOTE_TAGS", &self.evernote_tags)
    }
}

fn require_non_empty(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(anyhow!("{name} must not be empty"));
    }
    Ok(())
}

fn parse_comma_separated(name: &str, value: &str) -> Result<Vec<String>> {
    let items = value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if items.is_empty() {
        return Err(anyhow!("{name} must contain at least one non-empty value"));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_settings() -> Settings {
        Settings {
            yandex_music_token: "yandex-token".to_string(),
            evernote_auth_token: "evernote-token".to_string(),
            evernote_note_store_url: None,
            evernote_user_store_url: DEFAULT_EVERNOTE_USER_STORE_URL.to_string(),
            evernote_notebook_guid: None,
            evernote_tags: DEFAULT_EVERNOTE_TAG.to_string(),
            state_path: "state.json".into(),
            dry_run: false,
            max_tracks_per_run: 10,
            enrich_external_links: true,
            genius_access_token: None,
            songlink_user_country: "US".to_string(),
        }
    }

    #[test]
    fn note_store_url_is_optional() {
        let settings = base_settings().validate().expect("valid settings");

        assert_eq!(settings.evernote_note_store_url, None);
        assert_eq!(
            settings.evernote_user_store_url,
            DEFAULT_EVERNOTE_USER_STORE_URL
        );
        assert_eq!(
            settings.evernote_tag_names().expect("tags"),
            vec![DEFAULT_EVERNOTE_TAG]
        );
    }

    #[test]
    fn empty_note_store_url_is_treated_as_missing() {
        let mut settings = base_settings();
        settings.evernote_note_store_url = Some("  ".to_string());

        let settings = settings.validate().expect("valid settings");

        assert_eq!(settings.evernote_note_store_url, None);
    }

    #[test]
    fn parses_comma_separated_evernote_tags() {
        let mut settings = base_settings();
        settings.evernote_tags = "music, liked tracks, evernote ".to_string();

        let settings = settings.validate().expect("valid settings");

        assert_eq!(
            settings.evernote_tag_names().expect("tags"),
            vec!["music", "liked tracks", "evernote"]
        );
        assert_eq!(settings.evernote_tags, "music,liked tracks,evernote");
    }

    #[test]
    fn rejects_empty_evernote_tags() {
        let mut settings = base_settings();
        settings.evernote_tags = " , ".to_string();

        let error = settings.validate().expect_err("invalid tags");

        assert_eq!(
            error.to_string(),
            "EVERNOTE_TAGS must contain at least one non-empty value"
        );
    }
}

# Yandex Music Likes to Evernote

[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)
[![Reliability Rating](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=reliability_rating)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)
[![Security Rating](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=security_rating)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)
[![Maintainability Rating](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=sqale_rating)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)
[![Coverage](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=coverage)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)
[![Duplicated Lines (%)](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=duplicated_lines_density)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)
[![Bugs](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=bugs)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)
[![Vulnerabilities](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=vulnerabilities)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)
[![Code Smells](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=code_smells)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)
[![Lines of Code](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=ncloc)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)
[![Technical Debt](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_yandex-music-likes-to-evernote&metric=sqale_index)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_yandex-music-likes-to-evernote)

Rust CLI for a scheduled GitHub Actions job that exports newly liked Yandex Music tracks to Evernote as metadata notes.

It does **not** download or copy Yandex Music catalog audio. Each Evernote note contains track metadata and links back to Yandex Music.

## Configuration

Set these GitHub Actions repository secrets:

- `YANDEX_MUSIC_TOKEN`: Yandex Music OAuth token.
- `EVERNOTE_AUTH_TOKEN`: Evernote OAuth/developer auth token with note write access.
- `GENIUS_ACCESS_TOKEN`: optional Genius API token. Without it, the tool adds a Genius search link instead of resolving a matched song URL.

Optional GitHub Actions repository secrets or variables:

- `EVERNOTE_NOTE_STORE_URL`: account-specific Evernote NoteStore URL. If omitted, the tool asks Evernote UserStore for the right NoteStore URL using `EVERNOTE_AUTH_TOKEN`.
- `EVERNOTE_TAGS`: comma-separated Evernote tags to apply to created notes. Defaults to `yandex-music`.

Optional GitHub Actions repository variables:

- `EVERNOTE_USER_STORE_URL`: Evernote UserStore URL used for NoteStore discovery. Defaults to `https://www.evernote.com/edam/user`.
- `EVERNOTE_NOTEBOOK_GUID`: target notebook GUID or exact notebook name. If omitted, Evernote uses the default notebook.
- `STATE_PATH`: state JSON file path. Defaults to `state.json`.
- `DRY_RUN`: set to `true` to print notes without creating them.
- `MAX_TRACKS_PER_RUN`: cap created notes per run. Set to `0` to disable the cap. Defaults to `30`.
- `ENRICH_EXTERNAL_LINKS`: add external MusicBrainz, LRCLIB, Songlink/Odesli, Wikidata, Wikipedia, YouTube, and Genius links. Defaults to `true`.
- `SONGLINK_USER_COUNTRY`: optional two-letter country code for Songlink/Odesli lookup. Defaults to `US`.

External enrichment sends artist/title/album/link metadata to the selected public services. It never copies lyrics into Evernote.

## First Run

The first run is limited by `MAX_TRACKS_PER_RUN`, not by age. With the default configuration, it creates notes for up to `30` previously unprocessed liked tracks. Set `MAX_TRACKS_PER_RUN=0` if you want a full initial backfill.

## GitHub Actions Schedule

The workflow in `.github/workflows/sync.yml` runs daily at `03:17` UTC and can also be started manually from the GitHub Actions tab.

The job restores the latest `state.json` from the GitHub Actions cache, then saves the updated state under a fresh cache key. It also uploads `state.json` as a workflow artifact for inspection. This state file is good enough for one scheduled personal job. It is not a transactional database, so do not run multiple schedules for the same Evernote account in parallel.

GitHub-hosted Actions minutes are free for public repositories. Private repositories still use the account's included minutes quota.

## Local Run

```bash
cp .env.example .env
$EDITOR .env
cargo run -- sync
```

Use dry-run mode first:

```bash
DRY_RUN=true cargo run -- sync
```

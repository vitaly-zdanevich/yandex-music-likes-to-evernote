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

Rust CLI for a scheduled GitHub Actions job that backs up newly liked Yandex Music tracks to Evernote, one note per track.

Each note contains the track metadata and links back to Yandex Music. By default it **also attaches the track's audio file** to the note for personal backup: downloaded in the highest quality the account is entitled to and stored **without re-encoding** (lossless FLAC when your Yandex Plus tier allows it, otherwise the best available lossy stream). Set `BACKUP_AUDIO=false` for metadata-only notes.

## Configuration

Set these GitHub Actions repository secrets:

- `YANDEX_MUSIC_TOKEN`: Yandex Music OAuth token.
- `EVERNOTE_AUTH_TOKEN`: Evernote OAuth/developer auth token with note write access.
- `GENIUS_ACCESS_TOKEN`: optional Genius API token from [Genius API Clients](https://genius.com/api-clients). Without it, the tool adds a Genius search link instead of resolving a matched song URL.
- `ACOUSTID_API_KEY`: optional AcoustID application API key from [AcoustID](https://acoustid.org/new-application). When set together with `BACKUP_AUDIO=true`, the tool fingerprints the downloaded audio with `fpcalc` and tries to add exact MusicBrainz recording, artist, and album links. Without it, the tool uses MusicBrainz metadata search links.

Optional GitHub Actions repository secrets or variables:

- `EVERNOTE_NOTE_STORE_URL`: account-specific Evernote NoteStore URL. If omitted, the tool asks Evernote UserStore for the right NoteStore URL using `EVERNOTE_AUTH_TOKEN`.
- `EVERNOTE_TAGS`: comma-separated Evernote tags to apply to created notes. Defaults to `yandex-music`.

Optional GitHub Actions repository variables:

- `EVERNOTE_USER_STORE_URL`: Evernote UserStore URL used for NoteStore discovery. Defaults to `https://www.evernote.com/edam/user`.
- `EVERNOTE_NOTEBOOK_GUID`: target notebook GUID or exact notebook name. If omitted, Evernote uses the default notebook.
- `STATE_PATH`: state JSON file path. Defaults to `state.json`.
- `DRY_RUN`: set to `true` to print notes without creating them.
- `MAX_TRACKS_PER_RUN`: cap created notes per run. Set to `0` to disable the cap. Defaults to `30`.
- `BACKUP_AUDIO`: download each track's audio in the highest available quality (no re-encoding) and attach it to its note. Defaults to `true`. Lossless FLAC requires a Yandex Plus subscription tier that grants it; otherwise the best available lossy stream is used. Attachments count against your Evernote note-size and monthly upload limits, so set this to `false` if you want metadata-only notes. If a download fails transiently, the track is left unprocessed and retried on the next run rather than saved without audio. See [Audio backup](#audio-backup) for the resulting file formats.
- `ENRICH_EXTERNAL_LINKS`: add external AcoustID/MusicBrainz, LRCLIB, Songlink/Odesli, Spotify, Apple Music, Deezer, Bandcamp, SoundCloud, Discogs, TIDAL, Qobuz, Amazon Music, YouTube Music, YouTube, Last.fm, RuTracker, Beatport, WhoSampled, SecondHandSongs, AllMusic, ListenBrainz, TheAudioDB, VK, Rutube, Vimeo, Google, DuckDuckGo, Bing, Yandex, Yandex Video, Wikidata, Wikipedia, and Genius links. Defaults to `true`.
- `ENABLED_EXTERNAL_LINK_SERVICES`: optional comma-separated whitelist. If non-empty, only listed services are used.
- `DISABLED_EXTERNAL_LINK_SERVICES`: optional comma-separated blocklist. Applied after `ENABLED_EXTERNAL_LINK_SERVICES`, so it can remove services from a whitelist.
- `SONGLINK_USER_COUNTRY`: optional two-letter country code for Songlink/Odesli lookup. Defaults to `US`.

Service ids for the whitelist/blocklist: `acoustid`, `allmusic`, `amazonmusic`, `applemusic`, `bandcamp`, `beatport`, `bing`, `deezer`, `discogs`, `duckduckgo`, `genius`, `google`, `lastfm`, `listenbrainz`, `lrclib`, `musicbrainz`, `qobuz`, `rutracker`, `rutube`, `secondhandsongs`, `songlink`, `soundcloud`, `spotify`, `theaudiodb`, `tidal`, `vimeo`, `vk`, `wikidata`, `wikipedia`, `yandex`, `yandexvideo`, `youtube`, `youtubemusic`. Punctuation and case are ignored, so `last.fm`, `youtube-music`, `yandex-video`, and `VK` are accepted.

External enrichment sends artist/title/album/link metadata to the selected public services. AcoustID sends the local audio fingerprint and duration, not the audio file itself. It never copies lyrics into Evernote.

When `ACOUSTID_API_KEY` is present in GitHub Actions, the sync workflow installs `fpcalc` automatically from `libchromaprint-tools`. For local AcoustID lookup, install the same Chromaprint tool yourself and make sure `fpcalc` is in `PATH`.

## Audio backup

With `BACKUP_AUDIO` enabled (the default), each note carries the track's audio downloaded in the highest quality the account is entitled to and stored byte-for-byte — no re-encoding. The file format therefore mirrors whatever Yandex Music serves:

- **Lossless** → FLAC inside an MP4 container, saved as `.mp4` (`audio/mp4`). Yandex Music does not expose a native `.flac` stream, so a lossless track is a `.mp4` file whose audio is bit-exact FLAC. Needs a Yandex Plus tier that grants lossless.
- **High lossy** → AAC in MP4, saved as `.m4a` (`audio/mp4`).
- **Standard** → MP3, saved as `.mp3` (`audio/mpeg`).

Availability is per track: even with Yandex Plus some tracks only offer AAC or MP3, and the highest available is used. A lossless track is typically 30–70 MB, so on a large backfill keep an eye on your Evernote monthly upload quota (the per-run cap throttles this). You can preview what your account is served with the [audio smoke test](#audio-smoke-test).

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

### Audio smoke test

To check the audio download path against the live API without creating any Evernote notes, set `YANDEX_MUSIC_TOKEN` in `.env` and run the `#[ignore]`d smoke test. It downloads one liked track (verifying format/size/integrity) and summarizes the quality your account is served across a sample of the library — useful after a subscription change:

```bash
cargo test live_audio_smoke -- --ignored --nocapture
```

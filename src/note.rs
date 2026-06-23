use html_escape::encode_safe;

use crate::audio::{AudioAttachment, CoverAttachment};
use crate::enrichment::ExternalLink;
use crate::yandex::LikedTrack;

pub fn title(track: &LikedTrack) -> String {
    let artists = display_list(&track.artists);
    if artists.is_empty() {
        track.title.clone()
    } else {
        format!("{artists} - {}", track.title)
    }
}

pub fn enml(
    track: &LikedTrack,
    external_links: &[ExternalLink],
    cover: Option<&CoverAttachment>,
    audio: Option<&AudioAttachment>,
) -> String {
    let liked_at_rfc3339 = track.liked_at.to_rfc3339();
    let title = encode_safe(&track.title);
    let artists = render_artists(track);
    let albums = render_albums(track);
    let liked_at = encode_safe(&liked_at_rfc3339);
    let duration = track
        .duration_ms
        .map(format_duration)
        .map(|duration| format!("<div><b>Duration:</b> {}</div>", encode_safe(&duration)))
        .unwrap_or_default();
    let cover = cover.map(render_cover).unwrap_or_default();
    let external_links = render_external_links(external_links);
    let audio = audio
        .map(|audio| render_audio(audio, track.duration_ms))
        .unwrap_or_default();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE en-note SYSTEM "http://xml.evernote.com/pub/enml2.dtd">
<en-note>
<div><b>Track:</b> {title}</div>
<div><b>Album:</b> {albums}</div>
<div><b>Artist:</b> {artists}</div>
{duration}
<div><b>Liked at:</b> {liked_at}</div>
{cover}
{audio}
{external_links}
</en-note>"#
    )
}

fn render_audio(audio: &AudioAttachment, duration_ms: Option<u128>) -> String {
    let mime = encode_safe(&audio.mime);
    let hash = audio.md5_hex();

    // Yandex reports no bitrate for lossless flac-mp4, so fall back to the average
    // bitrate derived from size and duration (marked with `~`) instead of "0 kbps".
    let mut details = vec![encode_safe(&audio.quality).into_owned()];
    if let Some((kbps, estimated)) = audio.display_bitrate_kbps(duration_ms) {
        let prefix = if estimated { "~" } else { "" };
        details.push(format!("{prefix}{kbps} kbps"));
    }
    let details = details.join(", ");

    format!("<div><b>Audio:</b> {details}</div>\n<en-media type=\"{mime}\" hash=\"{hash}\"/>")
}

fn render_cover(cover: &CoverAttachment) -> String {
    let mime = encode_safe(&cover.mime);
    let hash = cover.md5_hex();

    format!("<en-media type=\"{mime}\" hash=\"{hash}\"/>")
}

fn display_list(items: &[String]) -> String {
    items.join(", ")
}

fn format_duration(duration_ms: u128) -> String {
    let total_seconds = duration_ms / 1000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes}:{seconds:02}")
}

fn render_artists(track: &LikedTrack) -> String {
    if track.artist_links.is_empty() {
        return encode_safe(&display_list(&track.artists)).into_owned();
    }

    track
        .artists
        .iter()
        .map(|artist| {
            if let Some(link) = track.artist_links.iter().find(|link| link.name == *artist) {
                let name = encode_safe(artist);
                let url = encode_safe(&link.yandex_url);
                format!("<a href=\"{url}\">{name}</a>")
            } else {
                encode_safe(artist).into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_albums(track: &LikedTrack) -> String {
    if track.album_links.is_empty() {
        return encode_safe(&display_list(&track.albums)).into_owned();
    }

    track
        .albums
        .iter()
        .map(|album| {
            if let Some(link) = track.album_links.iter().find(|link| link.name == *album) {
                let name = encode_safe(album);
                let url = encode_safe(&link.yandex_url);
                format!("<a href=\"{url}\">{name}</a>")
            } else {
                encode_safe(album).into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_external_links(links: &[ExternalLink]) -> String {
    if links.is_empty() {
        return String::new();
    }

    let rows = links
        .iter()
        .map(|link| {
            let label = encode_safe(&link.label);
            let url = encode_safe(&link.url);
            format!("<div><a href=\"{url}\">{label}</a></div>")
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!("<div><br/></div>\n{rows}")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::audio::{CoverImage, TrackAudio};
    use crate::yandex::{AlbumLink, ArtistLink};

    #[test]
    fn renders_enml_with_escaped_values() {
        let track = LikedTrack {
            id: "1".to_string(),
            liked_at: chrono::Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap(),
            title: "A < B".to_string(),
            artists: vec!["Artist & Co".to_string()],
            artist_links: vec![ArtistLink {
                name: "Artist & Co".to_string(),
                yandex_url: "https://music.yandex.com/artist/42".to_string(),
            }],
            albums: vec!["Album".to_string()],
            album_links: vec![AlbumLink {
                name: "Album".to_string(),
                yandex_url: "https://music.yandex.com/album/43".to_string(),
            }],
            duration_ms: Some(123_000),
            cover_url: Some("https://example.com/a?b=1&c=2".to_string()),
            yandex_url: "https://music.yandex.com/track/1".to_string(),
        };

        let links = vec![ExternalLink {
            label: "MusicBrainz recording search".to_string(),
            url: "https://musicbrainz.org/search?query=a&b=c".to_string(),
        }];
        let cover = CoverAttachment::new(
            CoverImage::new(b"cover".to_vec(), Some("image/jpeg")).expect("cover image"),
        );
        let enml = enml(&track, &links, Some(&cover), None);

        assert!(enml.contains("A &lt; B"));
        assert!(!enml.contains("<b>Track ID:</b>"));
        assert!(enml.contains(
            r#"<div><b>Artist:</b> <a href="https:&#x2F;&#x2F;music.yandex.com&#x2F;artist&#x2F;42">Artist &amp; Co</a></div>"#
        ));
        assert!(enml.contains(
            r#"<div><b>Album:</b> <a href="https:&#x2F;&#x2F;music.yandex.com&#x2F;album&#x2F;43">Album</a></div>"#
        ));
        let track_index = enml.find("<b>Track:</b>").expect("track line");
        let album_index = enml.find("<b>Album:</b>").expect("album line");
        let artist_index = enml.find("<b>Artist:</b>").expect("artist line");
        assert!(track_index < album_index);
        assert!(album_index < artist_index);
        assert!(!enml.contains("<b>Cover:</b>"));
        assert!(enml.contains(r#"<en-media type="image&#x2F;jpeg""#));
        assert!(!enml.contains(r#"<a href="https:&#x2F;&#x2F;example.com&#x2F;a?b=1&amp;c=2""#));
        assert!(enml.contains("2:03"));
        assert!(!enml.contains("External links"));
        assert!(enml.contains("MusicBrainz recording search"));
        assert!(enml.contains("musicbrainz.org"));
        assert!(!enml.contains("<b>Yandex Music:</b>"));
        assert!(!enml.contains("Audio is not copied by this tool"));
    }

    #[test]
    fn renders_minimal_note_without_optional_sections() {
        let track = LikedTrack {
            id: "2".to_string(),
            liked_at: chrono::Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap(),
            title: "Solo Track".to_string(),
            artists: Vec::new(),
            artist_links: Vec::new(),
            albums: Vec::new(),
            album_links: Vec::new(),
            duration_ms: None,
            cover_url: None,
            yandex_url: "https://music.yandex.com/track/2".to_string(),
        };

        let enml = enml(&track, &[], None, None);

        assert_eq!(title(&track), "Solo Track");
        assert!(enml.contains("<div><b>Track:</b> Solo Track</div>"));
        assert!(!enml.contains("<b>Duration:</b>"));
        assert!(!enml.contains("<b>Cover:</b>"));
        assert!(!enml.contains("<b>External links:</b>"));
        assert!(!enml.contains("<b>Audio:</b>"));
        assert!(!enml.contains("<en-media"));
    }

    #[test]
    fn renders_audio_attachment_media_tag() {
        let track = LikedTrack {
            id: "3".to_string(),
            liked_at: chrono::Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap(),
            title: "Song".to_string(),
            artists: vec!["Artist".to_string()],
            artist_links: Vec::new(),
            albums: Vec::new(),
            album_links: Vec::new(),
            duration_ms: None,
            cover_url: None,
            yandex_url: "https://music.yandex.com/track/3".to_string(),
        };
        let audio = AudioAttachment::new(
            TrackAudio {
                bytes: b"hello".to_vec(),
                codec: "flac".to_string(),
                bitrate_kbps: 1411,
                quality: "lossless".to_string(),
            },
            &title(&track),
        );

        let enml = enml(&track, &[], None, Some(&audio));

        assert!(enml.contains("<b>Audio:</b> lossless, 1411 kbps</div>"));
        assert!(!enml.contains("Artist - Song.flac"));
        assert!(!enml.contains("5 B"));
        assert!(enml.contains(
            r#"<en-media type="audio&#x2F;flac" hash="5d41402abc4b2a76b9719d911017c592"/>"#
        ));
    }

    #[test]
    fn omits_bitrate_when_unknown_and_duration_missing() {
        let track = LikedTrack {
            id: "4".to_string(),
            liked_at: chrono::Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap(),
            title: "Ethnicolor".to_string(),
            artists: vec!["Jean-Michel Jarre".to_string()],
            artist_links: Vec::new(),
            albums: Vec::new(),
            album_links: Vec::new(),
            duration_ms: None,
            cover_url: None,
            yandex_url: "https://music.yandex.com/track/4".to_string(),
        };
        let audio = AudioAttachment::new(
            TrackAudio {
                bytes: b"abc".to_vec(),
                codec: "flac-mp4".to_string(),
                bitrate_kbps: 0,
                quality: "lossless".to_string(),
            },
            &title(&track),
        );

        let enml = enml(&track, &[], None, Some(&audio));

        assert!(enml.contains("<b>Audio:</b> lossless</div>"));
        assert!(!enml.contains("Jean-Michel Jarre - Ethnicolor.mp4"));
        assert!(!enml.contains("3 B"));
        assert!(!enml.contains("kbps"));
        assert!(enml.contains(r#"<en-media type="audio&#x2F;mp4""#));
    }

    #[test]
    fn estimates_lossless_bitrate_from_duration() {
        let track = LikedTrack {
            id: "5".to_string(),
            liked_at: chrono::Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap(),
            title: "Ethnicolor".to_string(),
            artists: vec!["Jean-Michel Jarre".to_string()],
            artist_links: Vec::new(),
            albums: Vec::new(),
            album_links: Vec::new(),
            duration_ms: Some(1_000),
            cover_url: None,
            yandex_url: "https://music.yandex.com/track/5".to_string(),
        };
        let audio = AudioAttachment::new(
            TrackAudio {
                // 100000 bytes * 8 bits / 1000 ms = 800 kbps.
                bytes: vec![0u8; 100_000],
                codec: "flac-mp4".to_string(),
                bitrate_kbps: 0,
                quality: "lossless".to_string(),
            },
            &title(&track),
        );

        let enml = enml(&track, &[], None, Some(&audio));

        assert!(
            enml.contains("<b>Audio:</b> lossless, ~800 kbps</div>"),
            "got: {enml}"
        );
    }
}

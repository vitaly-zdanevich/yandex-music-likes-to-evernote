use html_escape::encode_safe;

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

pub fn enml(track: &LikedTrack, external_links: &[ExternalLink]) -> String {
    let album_list = display_list(&track.albums);
    let liked_at_rfc3339 = track.liked_at.to_rfc3339();
    let track_id = encode_safe(&track.id);
    let title = encode_safe(&track.title);
    let artists = render_artists(track);
    let albums = encode_safe(&album_list);
    let liked_at = encode_safe(&liked_at_rfc3339);
    let url = encode_safe(&track.yandex_url);
    let duration = track
        .duration_ms
        .map(format_duration)
        .map(|duration| format!("<div><b>Duration:</b> {}</div>", encode_safe(&duration)))
        .unwrap_or_default();
    let cover = track
        .cover_url
        .as_deref()
        .map(|cover_url| {
            let cover_url = encode_safe(cover_url);
            format!("<div><b>Cover:</b> <a href=\"{cover_url}\">{cover_url}</a></div>")
        })
        .unwrap_or_default();
    let external_links = render_external_links(external_links);

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE en-note SYSTEM "http://xml.evernote.com/pub/enml2.dtd">
<en-note>
<div><b>Track ID:</b> {track_id}</div>
<div><b>Track:</b> {title}</div>
<div><b>Artist:</b> {artists}</div>
<div><b>Album:</b> {albums}</div>
{duration}
<div><b>Liked at:</b> {liked_at}</div>
<div><b>Yandex Music:</b> <a href="{url}">{url}</a></div>
{cover}
{external_links}
</en-note>"#
    )
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

    format!("<div><br/></div>\n<div><b>External links:</b></div>\n{rows}")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::yandex::ArtistLink;

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
            duration_ms: Some(123_000),
            cover_url: Some("https://example.com/a?b=1&c=2".to_string()),
            yandex_url: "https://music.yandex.com/track/1".to_string(),
        };

        let links = vec![ExternalLink {
            label: "MusicBrainz recording search".to_string(),
            url: "https://musicbrainz.org/search?query=a&b=c".to_string(),
        }];
        let enml = enml(&track, &links);

        assert!(enml.contains("A &lt; B"));
        assert!(enml.contains("<div><b>Track ID:</b> 1</div>"));
        assert!(enml.contains(
            r#"<div><b>Artist:</b> <a href="https:&#x2F;&#x2F;music.yandex.com&#x2F;artist&#x2F;42">Artist &amp; Co</a></div>"#
        ));
        assert!(enml.contains("2:03"));
        assert!(enml.contains("External links"));
        assert!(enml.contains("MusicBrainz recording search"));
        assert!(enml.contains("musicbrainz.org"));
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
            duration_ms: None,
            cover_url: None,
            yandex_url: "https://music.yandex.com/track/2".to_string(),
        };

        let enml = enml(&track, &[]);

        assert_eq!(title(&track), "Solo Track");
        assert!(enml.contains("<div><b>Track:</b> Solo Track</div>"));
        assert!(!enml.contains("<b>Duration:</b>"));
        assert!(!enml.contains("<b>Cover:</b>"));
        assert!(!enml.contains("<b>External links:</b>"));
    }
}

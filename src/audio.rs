use std::fmt::Write as _;

/// Raw audio downloaded from Yandex Music and stored exactly as received, i.e.
/// without any re-encoding. `codec`, `bitrate_kbps`, and `quality` describe what
/// Yandex Music actually served so the original format is preserved verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackAudio {
    pub bytes: Vec<u8>,
    pub codec: String,
    pub bitrate_kbps: u32,
    pub quality: String,
}

impl TrackAudio {
    /// File extension for the container, derived from the Yandex codec name.
    pub fn extension(&self) -> &'static str {
        codec_extension(&self.codec)
    }

    /// MIME type for the container, derived from the Yandex codec name.
    pub fn mime(&self) -> &'static str {
        codec_mime(&self.codec)
    }
}

/// Audio prepared for attaching to an Evernote note as an `<en-media>` resource.
pub struct AudioAttachment {
    pub body: Vec<u8>,
    pub mime: String,
    pub file_name: String,
    pub bitrate_kbps: u32,
    pub quality: String,
    md5: [u8; 16],
}

impl AudioAttachment {
    /// Prepare downloaded audio for a note, naming the file after the note title
    /// and computing the MD5 that Evernote uses to link the resource.
    pub fn new(audio: TrackAudio, base_name: &str) -> Self {
        let mime = audio.mime().to_string();
        let file_name = format!("{}.{}", sanitize_file_stem(base_name), audio.extension());
        let md5 = md5::compute(&audio.bytes).0;
        Self {
            body: audio.bytes,
            mime,
            file_name,
            bitrate_kbps: audio.bitrate_kbps,
            quality: audio.quality,
            md5,
        }
    }

    /// Raw MD5 digest, as Evernote expects in `Data.bodyHash`.
    pub fn md5_raw(&self) -> Vec<u8> {
        self.md5.to_vec()
    }

    /// Hex-encoded MD5 digest, as Evernote expects in the `<en-media hash="...">` attribute.
    pub fn md5_hex(&self) -> String {
        let mut hex = String::with_capacity(self.md5.len() * 2);
        for byte in self.md5 {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    pub fn size(&self) -> usize {
        self.body.len()
    }

    pub fn human_size(&self) -> String {
        human_size(self.body.len())
    }

    /// Bitrate to display, in kbps. Returns the value Yandex reported, or—when it
    /// reports none (lossless flac-mp4 comes back with bitrate 0)—the average
    /// bitrate derived from the file size and track duration. The bool marks an
    /// estimated (averaged) value.
    pub fn display_bitrate_kbps(&self, duration_ms: Option<u128>) -> Option<(u32, bool)> {
        if self.bitrate_kbps > 0 {
            return Some((self.bitrate_kbps, false));
        }
        let duration_ms = duration_ms.filter(|ms| *ms > 0)?;
        if self.body.is_empty() {
            return None;
        }
        // bytes * 8 bits / (duration_ms / 1000) s / 1000 == bytes * 8 / duration_ms (kbps).
        let kbps = (self.body.len() as u128 * 8) / duration_ms;
        u32::try_from(kbps)
            .ok()
            .filter(|kbps| *kbps > 0)
            .map(|kbps| (kbps, true))
    }
}

fn codec_extension(codec: &str) -> &'static str {
    match codec {
        "flac" => "flac",
        "flac-mp4" => "mp4",
        "mp3" => "mp3",
        "aac" | "aac-mp4" | "he-aac" | "he-aac-mp4" => "m4a",
        _ => "bin",
    }
}

fn codec_mime(codec: &str) -> &'static str {
    match codec {
        "flac" => "audio/flac",
        "flac-mp4" => "audio/mp4",
        "mp3" => "audio/mpeg",
        "aac" | "aac-mp4" | "he-aac" | "he-aac-mp4" => "audio/mp4",
        _ => "application/octet-stream",
    }
}

/// Make `name` safe to use as a file name: replace characters that are illegal on
/// common filesystems, collapse whitespace, and keep the length sane.
fn sanitize_file_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }

    let cleaned = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned = cleaned.trim_matches('.').trim();
    if cleaned.is_empty() {
        return "track".to_string();
    }

    cleaned
        .chars()
        .take(120)
        .collect::<String>()
        .trim()
        .to_string()
}

fn human_size(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= MIB {
        format!("{:.1} MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lossless_audio(bytes: Vec<u8>) -> TrackAudio {
        TrackAudio {
            bytes,
            codec: "flac".to_string(),
            bitrate_kbps: 0,
            quality: "lossless".to_string(),
        }
    }

    #[test]
    fn maps_codecs_to_extension_and_mime() {
        let cases = [
            ("flac", "flac", "audio/flac"),
            ("flac-mp4", "mp4", "audio/mp4"),
            ("mp3", "mp3", "audio/mpeg"),
            ("aac", "m4a", "audio/mp4"),
            ("aac-mp4", "m4a", "audio/mp4"),
            ("he-aac", "m4a", "audio/mp4"),
            ("unknown", "bin", "application/octet-stream"),
        ];
        for (codec, extension, mime) in cases {
            let audio = TrackAudio {
                bytes: Vec::new(),
                codec: codec.to_string(),
                bitrate_kbps: 0,
                quality: "lossless".to_string(),
            };
            assert_eq!(audio.extension(), extension, "extension for {codec}");
            assert_eq!(audio.mime(), mime, "mime for {codec}");
        }
    }

    #[test]
    fn builds_attachment_with_sanitized_name_and_md5() {
        let attachment = AudioAttachment::new(lossless_audio(b"hello".to_vec()), "AC/DC - T:N*S?");

        assert_eq!(attachment.file_name, "AC_DC - T_N_S_.flac");
        assert_eq!(attachment.mime, "audio/flac");
        assert_eq!(attachment.size(), 5);
        // MD5 of "hello".
        assert_eq!(attachment.md5_hex(), "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(attachment.md5_raw().len(), 16);
    }

    #[test]
    fn falls_back_to_track_when_name_is_empty() {
        let attachment = AudioAttachment::new(lossless_audio(vec![0u8; 3]), "  ///  ");
        assert_eq!(attachment.file_name, "___.flac");

        let attachment = AudioAttachment::new(lossless_audio(vec![0u8; 3]), "   ");
        assert_eq!(attachment.file_name, "track.flac");
    }

    #[test]
    fn formats_human_size() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MiB");
    }
}

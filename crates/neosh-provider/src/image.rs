//! Reading an attached image at the moment a request is built.
//!
//! [`neosh_proto::ContentBlock::Image`] names a file rather than carrying one, so every driver
//! that can send a picture has the same small job: read the bytes and base64 them, in whatever
//! shape its wire format wants. The reading is here so there is one answer to what happens when
//! the file has gone.
//!
//! Which it can: the bytes live in the workspace's directory, and a workspace directory can be
//! cleared out between the day a conversation was had and the day it is reopened. A missing file
//! is *not* a failed turn — the rest of the message is still a question worth asking, and failing
//! it would mean an old conversation could never be replayed at all. The block is dropped, and the
//! driver that dropped it says so where the transcript can see it.

use base64::Engine as _;

/// The bytes of an attached image, base64-encoded, or nothing if it cannot be read.
pub fn base64_at(path: &str) -> Option<String> {
    match std::fs::read(path) {
        Ok(bytes) => Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
        Err(e) => {
            tracing::warn!(path, error = %e, "an attached image is no longer there; sending the turn without it");
            None
        }
    }
}

/// The `data:` URL form, which is what every OpenAI-shaped API wants instead of a source object.
pub fn data_url(path: &str, media_type: &str) -> Option<String> {
    Some(format!("data:{media_type};base64,{}", base64_at(path)?))
}

/// Write a picture a tool came back with into the workspace's store, and say where.
///
/// The counterpart of [`base64_at`]: that reads a file at request time, this writes one at result
/// time, and the two meet in the conversation as a path. The media type is read off the bytes
/// rather than trusted, because it names the file's extension and a `.png` that is really a JPEG
/// is the one thing a later reader of the file cannot recover from. Anything that is not one of
/// the four kinds every provider here accepts is dropped, with a log line: the tool's text is
/// still the result, and the picture was only ever a bonus.
pub fn keep(store: &std::path::Path, claimed: &str, base64: &str) -> Option<neosh_proto::ImageFile> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(base64.trim()) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "a tool returned an image that is not base64; dropping it");
            return None;
        }
    };
    let Some(media_type) = sniff(&bytes) else {
        tracing::warn!(claimed, "a tool returned an image of a kind nothing here can show; dropping it");
        return None;
    };
    if let Err(e) = std::fs::create_dir_all(store) {
        tracing::warn!(store = %store.display(), error = %e, "could not make the image store");
        return None;
    }
    let ext = media_type.strip_prefix("image/").unwrap_or("png");
    let path = store.join(format!("{}.{ext}", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&path, &bytes) {
        tracing::warn!(path = %path.display(), error = %e, "could not keep a tool's image");
        return None;
    }
    Some(neosh_proto::ImageFile { path: path.display().to_string(), media_type: media_type.to_string() })
}

/// What the bytes are, from the bytes. The four kinds every provider we target accepts.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One-pixel PNG, as a tool returns it: base64 in a block with a claimed media type.
    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    #[test]
    fn a_returned_picture_is_written_once_and_named_by_its_bytes() {
        let dir = std::env::temp_dir().join(format!("neosh-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Claimed as a JPEG; the bytes say PNG, and the bytes win.
        let kept = keep(&dir, "image/jpeg", PIXEL).expect("kept");
        assert_eq!(kept.media_type, "image/png");
        assert!(kept.path.ends_with(".png"), "{}", kept.path);
        assert_eq!(std::fs::read(&kept.path).map(|b| b.len()).ok(), Some(70));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_is_not_a_picture_is_dropped_rather_than_kept() {
        let dir = std::env::temp_dir().join(format!("neosh-keep-not-{}", std::process::id()));
        assert!(keep(&dir, "image/png", "aGVsbG8=").is_none(), "hello is not an image");
        assert!(keep(&dir, "image/png", "***").is_none(), "and that is not base64");
        assert!(!dir.exists(), "nothing was made for nothing");
    }
}

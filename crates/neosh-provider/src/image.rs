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

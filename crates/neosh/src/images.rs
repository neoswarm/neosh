//! Getting a picture out of the machine and into a message.
//!
//! Three ways in, because there are three ways people actually have an image to hand and only one
//! of them is a keystroke:
//!
//! * **the clipboard** ([`from_clipboard`]), which is where a screenshot is. This is the one that
//!   needs a key, because a terminal's own paste can only ever hand over *text* — bracketed paste
//!   is a text protocol, and no amount of pressing `⌘V` will make a PNG arrive down stdin. Every
//!   agent that does this shells out to the platform's clipboard tool for exactly that reason, and
//!   so does this.
//! * **a path that was pasted** ([`from_path`]), which is what dragging a file onto a terminal
//!   window does: the terminal pastes its path. Typing one has the same effect, which is the point
//!   — it is not a separate gesture to learn.
//! * **a plugin**, through the same [`from_path`], because the API is the product.
//!
//! # What is done to the bytes
//!
//! Read, sniffed, shrunk if it is enormous, and written once into the workspace's own directory.
//!
//! Sniffed rather than trusted: the media type has to be right or the provider rejects the turn,
//! and a `.png` that is really a JPEG is a thing that exists. The bytes say what they are in their
//! first few, and that is what is believed.
//!
//! Shrunk because a screenshot of a 4K display is eight megapixels and every provider we target
//! scales it down on arrival anyway — so the only thing full resolution buys is a slower request
//! and, on a metered connection, a worse one. [`MAX_EDGE`] is the long edge past which nothing is
//! gained. An image already inside it is written through *untouched*, which matters: re-encoding a
//! PNG that did not need it is a way to make a screenshot of text blurrier for nothing.

use std::path::{Path, PathBuf};

/// The long edge past which no provider we target keeps the extra pixels.
const MAX_EDGE: u32 = 1568;

/// What a request will carry. Well under every provider's per-image ceiling once base64 has added
/// its third, and far past anything a screenshot produces — this is the guard against somebody
/// pasting a RAW file, not a budget.
const MAX_BYTES: usize = 3_500_000;

/// An image the composer is holding, or a message is carrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Where the bytes are, in the workspace's directory.
    pub path: PathBuf,
    /// What they are, read off the bytes.
    pub media_type: String,
    /// For the chip. What is worth knowing about an attachment you cannot see is how big it is.
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

impl Attachment {
    /// What the chip says: `png 1920×1080 · 412 KB`.
    pub fn label(&self) -> String {
        let kind = self.media_type.strip_prefix("image/").unwrap_or(&self.media_type);
        format!("{kind} {}\u{d7}{} \u{b7} {}", self.width, self.height, human(self.bytes))
    }

    pub fn block(&self) -> neosh_proto::ContentBlock {
        neosh_proto::ContentBlock::Image {
            path: self.path.display().to_string(),
            media_type: self.media_type.clone(),
        }
    }
}

fn human(bytes: usize) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b / (1 << 10)),
        b => format!("{b} B"),
    }
}

/// What the bytes are, from the bytes.
///
/// Only the four types every provider we target accepts. Anything else — a TIFF, a PDF, a text
/// file somebody dragged in — is not an image as far as this is concerned, and saying so here is
/// what stops it becoming a turn that fails on the wire with somebody else's error message.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.starts_with(PNG) {
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

/// Take an image from a file and put a copy where the workspace keeps them.
///
/// A copy rather than a reference, and this is not paranoia: the path came from a paste, which
/// means it is very often `/tmp/Screenshot from 2026-08-21.png` or a file the next screenshot
/// overwrites. A conversation is reopened days later and replayed to a driver that reads the path
/// then — so what it reads has to still be the picture that was sent, and the only way to promise
/// that is to own the bytes.
pub fn from_path(store: &Path, path: &Path) -> Result<Attachment, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    keep(store, bytes)
}

/// Take whatever image is on the system clipboard.
///
/// Nothing here knows how to talk to a clipboard, and that is on purpose. Every desktop already
/// ships a program whose whole job this is, all four of them are present by default on the systems
/// they belong to, and shelling out to them is how the terminal agents that do this do it. A
/// library instead would mean linking X11 and Wayland into a program that spends its life on a
/// pipe, and would still be wrong on the machine where the workspace is headless.
///
/// Tried in order, first one that produces bytes wins. The error names what was missing, because
/// "no image on the clipboard" and "no tool installed to look" are different problems with
/// different fixes and only the second one is ours to explain.
pub fn from_clipboard(store: &Path) -> Result<Attachment, String> {
    let mut tried: Vec<&str> = Vec::new();
    for (program, args) in clipboard_readers() {
        if which(program).is_none() {
            continue;
        }
        tried.push(program);
        let out = std::process::Command::new(program)
            .args(args)
            .stderr(std::process::Stdio::null())
            .output();
        let Ok(out) = out else { continue };
        if !out.status.success() || out.stdout.is_empty() {
            continue;
        }
        // Some of these print a path rather than the picture — `osascript` asked for a file URL,
        // `powershell` after it has saved one. One line that names a file that exists is a path;
        // anything else is the bytes.
        if let Some(p) = as_path_line(&out.stdout) {
            return from_path(store, &p);
        }
        if sniff(&out.stdout).is_some() {
            return keep(store, out.stdout);
        }
    }
    if tried.is_empty() {
        return Err(format!(
            "no clipboard tool to read an image with \u{2014} install one of {}",
            clipboard_readers()
                .iter()
                .map(|(p, _)| *p)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Err("nothing on the clipboard that is an image".to_string())
}

/// The programs that can hand over a clipboard image, most specific first.
///
/// Wayland before X11 because a Wayland session usually has `xclip` too, through XWayland, looking
/// at a clipboard that is not the one you copied into.
fn clipboard_readers() -> Vec<(&'static str, Vec<&'static str>)> {
    if cfg!(target_os = "macos") {
        return vec![
            ("pngpaste", vec!["-"]),
            // Always present, so it is the floor rather than an extra to install. Asked for a
            // *file* URL: an image copied in Finder is a file reference, and this is the only one
            // of the two that can see it.
            ("osascript", vec!["-e", "get POSIX path of (the clipboard as \u{ab}class furl\u{bb})"]),
        ];
    }
    if cfg!(target_os = "windows") {
        return vec![("powershell", vec!["-NoProfile", "-Command", WINDOWS_DUMP])];
    }
    vec![
        ("wl-paste", vec!["--no-newline", "--type", "image/png"]),
        ("xclip", vec!["-selection", "clipboard", "-t", "image/png", "-o"]),
    ]
}

/// Save the clipboard image to a temporary PNG and print where it went. Nothing on the clipboard
/// exits non-zero, which is how the caller tells the two apart.
const WINDOWS_DUMP: &str = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
$img = Get-Clipboard -Format Image; if ($img -eq $null) { exit 1 }; \
$p = [System.IO.Path]::ChangeExtension([System.IO.Path]::GetTempFileName(), 'png'); \
$img.Save($p, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output $p";

/// A program's output read as "this is where the file is", or nothing.
fn as_path_line(out: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(out).ok()?.trim();
    if text.is_empty() || text.lines().count() != 1 {
        return None;
    }
    let path = PathBuf::from(text);
    path.is_file().then_some(path)
}

/// A single pasted line read as a path to an image, or nothing.
///
/// This is what makes dragging a file onto the terminal work, so it has to be *quiet*: anything it
/// is unsure about is ordinary text, because inserting what was typed is the behaviour somebody is
/// expecting and swallowing it is not. One line, one path, a file that exists, and bytes that are
/// an image — four gates, and a sentence with a space in it does not get past the second.
pub fn pasted_path(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    // Quoted or `file://`-wrapped, which is what a drag produces on most desktops.
    let bare = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| text.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(text);
    let bare = match bare.strip_prefix("file://") {
        // `file:///tmp/a.png` — the authority is empty for a local file, and percent-escapes are
        // how a space survives the trip.
        Some(rest) => unescape(rest.strip_prefix("localhost").unwrap_or(rest)),
        // A drag also escapes spaces the shell's way, which is what makes the path a single word.
        None => bare.replace("\\ ", " "),
    };
    let path = PathBuf::from(shellexpand::tilde(&bare).into_owned());
    if !path.is_file() {
        return None;
    }
    // The extension is a hint about whether it is worth reading, not the answer. The answer is in
    // the bytes, and it is `keep` that asks them.
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    matches!(ext.as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "webp")).then_some(path)
}

/// `%20` and friends, for a path that came through a `file://` URL.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let hex: String = chars.clone().take(2).collect();
        match u8::from_str_radix(&hex, 16) {
            Ok(b) => {
                out.push(b as char);
                chars.next();
                chars.next();
            }
            Err(_) => out.push(c),
        }
    }
    out
}

/// Sniff, shrink if it is worth shrinking, and write it where it will still be in a week.
fn keep(store: &Path, bytes: Vec<u8>) -> Result<Attachment, String> {
    let Some(media_type) = sniff(&bytes) else {
        return Err("that is not an image of a kind any model here can read".to_string());
    };
    let decoded = image::load_from_memory(&bytes)
        .map_err(|e| format!("could not read the image: {e}"))?;
    let (w, h) = (image::GenericImageView::dimensions(&decoded).0, image::GenericImageView::dimensions(&decoded).1);

    // Left alone when it is already small enough. Re-encoding costs sharpness on exactly the kind
    // of image people paste — a screenshot of text — and buys nothing.
    let (bytes, media_type, w, h) = if w <= MAX_EDGE && h <= MAX_EDGE && bytes.len() <= MAX_BYTES {
        (bytes, media_type.to_string(), w, h)
    } else {
        let small = decoded.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Lanczos3);
        let mut png = Vec::new();
        small
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| format!("could not re-encode the image: {e}"))?;
        let (sw, sh) = (image::GenericImageView::dimensions(&small).0, image::GenericImageView::dimensions(&small).1);
        (png, "image/png".to_string(), sw, sh)
    };
    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "that image is {} \u{2014} more than a turn can carry",
            human(bytes.len())
        ));
    }

    std::fs::create_dir_all(store)
        .map_err(|e| format!("could not make {}: {e}", store.display()))?;
    let ext = media_type.strip_prefix("image/").unwrap_or("png");
    let path = store.join(format!("{}.{ext}", uuid::Uuid::new_v4()));
    let len = bytes.len();
    std::fs::write(&path, bytes).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(Attachment { path, media_type, width: w, height: h, bytes: len })
}

/// Where a program is on `PATH`, or nothing.
fn which(program: &str) -> Option<PathBuf> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    std::env::var_os("PATH")?.to_str()?.split(sep).find_map(|dir| {
        let candidate = Path::new(dir).join(program);
        candidate.is_file().then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-pixel PNG, so the tests exercise the real decoder rather than a shape.
    fn pixel() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encode");
        out
    }

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("neosh-img-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn what_it_is_is_read_off_the_bytes_and_not_off_the_name() {
        assert_eq!(sniff(&pixel()), Some("image/png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0x00]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        // The one that matters: a provider told this was a PNG would reject the turn.
        assert_eq!(sniff(b"%PDF-1.4"), None, "a PDF is not one of the four");
        assert_eq!(sniff(b"hello"), None);
    }

    #[test]
    fn an_image_small_enough_already_is_written_through_untouched() {
        let dir = tmp("through");
        let bytes = pixel();
        let got = keep(&dir, bytes.clone()).expect("kept");
        assert_eq!(got.media_type, "image/png");
        assert_eq!((got.width, got.height), (1, 1));
        assert_eq!(
            std::fs::read(&got.path).expect("read back"),
            bytes,
            "re-encoding a screenshot of text is how it gets blurrier for nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn something_that_is_not_an_image_is_refused_here_rather_than_on_the_wire() {
        let dir = tmp("refuse");
        let err = keep(&dir, b"#!/bin/sh\necho hi\n".to_vec()).expect_err("refused");
        assert!(err.contains("not an image"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_huge_one_is_brought_down_to_a_size_worth_sending() {
        let dir = tmp("shrink");
        let big = image::RgbaImage::from_pixel(4000, 2000, image::Rgba([9, 9, 9, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(big)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode");
        let got = keep(&dir, bytes).expect("kept");
        assert_eq!(got.width, MAX_EDGE, "the long edge is what is clamped");
        assert_eq!(got.height, MAX_EDGE / 2, "and the aspect ratio survives it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pasted_path_is_only_a_path_when_it_is_unambiguously_one() {
        let dir = tmp("paste");
        let file = dir.join("shot.png");
        std::fs::write(&file, pixel()).expect("write");

        assert_eq!(pasted_path(file.to_str().expect("utf8")).as_ref(), Some(&file));
        // The three shapes a drag produces.
        assert_eq!(pasted_path(&format!("\"{}\"", file.display())).as_ref(), Some(&file));
        assert_eq!(pasted_path(&format!("'{}'", file.display())).as_ref(), Some(&file));
        assert_eq!(pasted_path(&format!("file://{}", file.display())).as_ref(), Some(&file));

        // And the things that are sentences. Every one of these would be text eaten by a gesture
        // nobody made.
        assert_eq!(pasted_path("have a look at shot.png"), None, "a sentence is not a path");
        assert_eq!(pasted_path(&format!("{} and this", file.display())), None);
        assert_eq!(pasted_path("/nope/missing.png"), None, "a path to nothing is text");
        assert_eq!(pasted_path(""), None);

        // A file that exists but is not one of the four we can send.
        let notes = dir.join("notes.txt");
        std::fs::write(&notes, "hello").expect("write");
        assert_eq!(pasted_path(notes.to_str().expect("utf8")), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_space_survives_both_ways_a_drag_can_escape_it() {
        let dir = tmp("space");
        let file = dir.join("my shot.png");
        std::fs::write(&file, pixel()).expect("write");
        assert_eq!(pasted_path(&format!("file://{}", file.display().to_string().replace(' ', "%20"))).as_ref(), Some(&file));
        assert_eq!(pasted_path(&file.display().to_string().replace(' ', "\\ ")).as_ref(), Some(&file));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_chip_says_the_things_you_cannot_see_for_yourself() {
        let a = Attachment {
            path: PathBuf::from("/x/y.png"),
            media_type: "image/png".into(),
            width: 1920,
            height: 1080,
            bytes: 421_888,
        };
        assert_eq!(a.label(), "png 1920\u{d7}1080 \u{b7} 412 KB");
    }
}

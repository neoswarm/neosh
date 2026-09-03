//! What the operating system refused, and what a person can actually do about it.
//!
//! Nothing here reads or writes anything. It takes an `io::Error` that has already come back from
//! somewhere and turns it into the one sentence that is worth putting on screen — which is a
//! different sentence on macOS than anywhere else, and that difference is the whole reason this
//! module exists.
//!
//! # Why macOS is its own answer
//!
//! On every other platform a directory you cannot read is a directory whose mode bits say so, and
//! `ls -l` is the diagnosis. On macOS it is usually not about the file at all: the privacy layer
//! (TCC) grants access to **the application**, and the application here is the terminal neosh is
//! running in. So `~/Documents` is refused because Ghostty or Terminal.app was never allowed to
//! look there — `chmod` changes nothing, the file's owner changes nothing, and `sudo`, which is
//! what everybody reaches for, changes nothing either and is the reason this wastes people's
//! afternoons. The fix is a checkbox in System Settings, against the terminal's name, and it is
//! not guessable from "permission denied".
//!
//! # The one bit the kernel gives us
//!
//! `EACCES` is the mode bits and `EPERM` is the privacy layer, and Rust maps **both** onto
//! `ErrorKind::PermissionDenied` — so the kind alone cannot tell them apart and the raw errno is
//! the only thing that can. Everything below hangs off that single number, which is why it is read
//! directly rather than through `kind()`.

use std::path::Path;

/// `EPERM` — the operation is not permitted. On macOS this is the privacy layer's refusal.
const EPERM: i32 = 1;
/// `EACCES` — ordinary Unix mode bits.
const EACCES: i32 = 13;

/// Why a path was refused, when the reason was a permission rather than an absence.
///
/// Two variants because they are two different problems with two different fixes, and telling
/// somebody to check the mode bits on a directory macOS is hiding from their terminal is worse
/// than saying nothing: it is a confident answer pointing away from the cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// macOS privacy. The grant belongs to the terminal, and the pane it is granted in depends on
    /// which folder was asked for.
    Privacy {
        /// The protected folder this path is in, when it is one we can name.
        folder: Option<&'static str>,
        /// The System Settings pane that grants it.
        pane: Pane,
    },
    /// Ordinary Unix mode bits, on any platform including macOS.
    Mode,
}

/// The System Settings pane a macOS grant lives in.
///
/// Two of them, because `~/Documents` and `~/Library/Mail` are refused by the same errno and
/// granted in different places — sending somebody to the wrong one is a fix that does not work
/// followed by no further ideas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// Desktop, Documents, Downloads, removable volumes, network volumes.
    FilesAndFolders,
    /// Mail, Messages, the Photos library, Time Machine — everything behind the big switch.
    FullDisk,
}

impl Pane {
    /// What the pane is called, as a person reading System Settings would find it.
    fn label(self) -> &'static str {
        match self {
            Self::FilesAndFolders => "Privacy & Security \u{203a} Files and Folders",
            Self::FullDisk => "Privacy & Security \u{203a} Full Disk Access",
        }
    }
}

/// The folders macOS protects, longest path first so `Library/Mail` is matched before `Library`.
///
/// Not exhaustive and cannot be: the list is Apple's and it grows. An unrecognised path still gets
/// the privacy sentence — it just does not get to name the folder, which is a detail rather than
/// the answer.
const PROTECTED: &[(&str, &str, Pane)] = &[
    ("Library/Mail", "Mail", Pane::FullDisk),
    ("Library/Messages", "Messages", Pane::FullDisk),
    ("Library/Safari", "Safari's data", Pane::FullDisk),
    ("Library/Application Support/com.apple.TCC", "the privacy database", Pane::FullDisk),
    ("Pictures/Photos Library.photoslibrary", "the Photos library", Pane::FullDisk),
    ("Desktop", "Desktop", Pane::FilesAndFolders),
    ("Documents", "Documents", Pane::FilesAndFolders),
    ("Downloads", "Downloads", Pane::FilesAndFolders),
    ("Library/Mobile Documents", "iCloud Drive", Pane::FilesAndFolders),
];

/// Why this error refused this path, or `None` if it was not a permission problem at all.
///
/// `None` for a missing directory, a broken symlink or anything else: those are ordinary answers
/// that the caller already has a better sentence for, and dressing them up as a permission is how
/// a diagnostic stops being trusted.
pub fn refused(err: &std::io::Error, path: &Path) -> Option<Refused> {
    match err.raw_os_error() {
        Some(EPERM) if cfg!(target_os = "macos") => {
            let (folder, pane) = protected(path);
            Some(Refused::Privacy { folder, pane })
        }
        // `EPERM` off macOS has no privacy layer behind it, so it is only ever the mode bits or
        // something wearing their clothes.
        Some(EPERM) | Some(EACCES) => Some(Refused::Mode),
        // A platform that reports permissions without an errno — and the kind is still worth
        // believing when it says this much.
        None if err.kind() == std::io::ErrorKind::PermissionDenied => Some(Refused::Mode),
        _ => None,
    }
}

/// Which protected folder a path is inside, and the pane that grants it.
///
/// Matched against the path relative to home, because these are all per-user folders and an
/// absolute match would miss every one of them under a different account.
fn protected(path: &Path) -> (Option<&'static str>, Pane) {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let relative = home.as_deref().and_then(|home| path.strip_prefix(home).ok());
    if let Some(relative) = relative {
        for (prefix, name, pane) in PROTECTED {
            if relative.starts_with(prefix) {
                return (Some(name), *pane);
            }
        }
    }
    // Unrecognised, so the safer of the two: Full Disk Access is the one that covers everything,
    // and pointing at the narrower pane for a folder it does not cover is advice that fails.
    (None, Pane::FullDisk)
}

impl Refused {
    /// The sentence to put on screen, naming what was refused and where it is granted.
    ///
    /// Deliberately one line. This appears in a picker row and in a notice, neither of which is a
    /// place to explain the macOS privacy model — the job is to name the fix precisely enough that
    /// somebody can go and do it.
    pub fn sentence(&self, what: &str) -> String {
        match self {
            Self::Privacy { folder, pane } => {
                let subject = match folder {
                    Some(folder) => format!("{folder} is protected by macOS"),
                    None => format!("macOS will not let neosh read {what}"),
                };
                format!(
                    "{subject} \u{2014} allow {} under System Settings \u{203a} {}",
                    terminal(),
                    pane.label()
                )
            }
            Self::Mode => format!("no permission to read {what}"),
        }
    }
}

/// What to call the application the privacy grant belongs to.
///
/// `TERM_PROGRAM` names the terminal that started *this process*, and a workspace outlives the
/// terminal that started it — so after a detach and a reattach from somewhere else this is a name
/// for a window that is not there any more. A confident wrong name is worse than a vague right
/// one, because it is the name somebody goes looking for in a list of thirty applications, so an
/// unset or unrecognised variable becomes "your terminal" rather than a guess.
pub fn terminal() -> String {
    let Ok(program) = std::env::var("TERM_PROGRAM") else {
        return "your terminal".to_string();
    };
    match program.as_str() {
        "Apple_Terminal" => "Terminal".to_string(),
        "iTerm.app" => "iTerm".to_string(),
        "vscode" => "Visual Studio Code".to_string(),
        // Ghostty, WezTerm, Alacritty, kitty and the rest already say their own name.
        "" => "your terminal".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(errno: i32) -> std::io::Error {
        std::io::Error::from_raw_os_error(errno)
    }

    #[test]
    fn the_two_refusals_the_kernel_reports_as_one_are_told_apart_by_errno() {
        // Both of these are `ErrorKind::PermissionDenied`, which is exactly why the errno is what
        // gets read: if this ever stops being true the whole module is answering the wrong
        // question, and it should fail here rather than on somebody's Mac.
        assert_eq!(err(EPERM).kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(err(EACCES).kind(), std::io::ErrorKind::PermissionDenied);

        let path = Path::new("/tmp/whatever");
        assert_eq!(refused(&err(EACCES), path), Some(Refused::Mode));
        if cfg!(target_os = "macos") {
            assert!(matches!(refused(&err(EPERM), path), Some(Refused::Privacy { .. })));
        } else {
            assert_eq!(refused(&err(EPERM), path), Some(Refused::Mode));
        }
    }

    #[test]
    fn a_missing_directory_is_not_a_permission_problem() {
        // 2 is ENOENT. The caller has a better sentence for this and must be allowed to use it.
        assert_eq!(refused(&err(2), Path::new("/nope")), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_protected_folder_is_named_and_sent_to_the_pane_that_grants_it() {
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());

        let docs = refused(&err(EPERM), &home.join("Documents/work"));
        assert_eq!(
            docs,
            Some(Refused::Privacy { folder: Some("Documents"), pane: Pane::FilesAndFolders })
        );

        // Longest-first matching: `Library/Mail` must not be answered by a bare `Library` rule,
        // and it is granted in the other pane.
        let mail = refused(&err(EPERM), &home.join("Library/Mail/V10"));
        assert_eq!(mail, Some(Refused::Privacy { folder: Some("Mail"), pane: Pane::FullDisk }));

        // Somewhere Apple protects that this list has never heard of still gets the privacy
        // sentence — it just cannot name the folder.
        let unknown = refused(&err(EPERM), Path::new("/Volumes/someone-elses-disk"));
        assert_eq!(unknown, Some(Refused::Privacy { folder: None, pane: Pane::FullDisk }));
    }

    #[test]
    fn the_sentence_says_where_to_go_rather_than_that_something_went_wrong() {
        let privacy =
            Refused::Privacy { folder: Some("Documents"), pane: Pane::FilesAndFolders }
                .sentence("~/Documents");
        assert!(privacy.contains("Documents is protected by macOS"), "{privacy}");
        assert!(privacy.contains("Files and Folders"), "{privacy}");
        // The fix names an application, because the grant is the application's and not the file's.
        assert!(privacy.contains("allow "), "{privacy}");

        // The ordinary case stays ordinary: no macOS lecture on a Linux box with a 0700 directory.
        let mode = Refused::Mode.sentence("/srv/private");
        assert_eq!(mode, "no permission to read /srv/private");
        assert!(!mode.contains("System Settings"));
    }
}

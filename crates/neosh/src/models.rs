//! Keeping the model catalogue current without a release.
//!
//! The catalogue is ours (`crates/neosh-provider/models.json`) with t3code's model manifest laid
//! over it (see `neosh_provider::manifest` and `neosh_provider::t3code`). Theirs is updated the day
//! a vendor ships, so a running workspace reads it again from their `main`, and a model released
//! this morning is in `^P` this afternoon on a binary built last month. This is the fetching half —
//! when, from where, and what is kept on disk between runs.
//!
//! # Cheap on purpose
//!
//! One small file, asked for every six hours and on `^R` in the model picker, with the `ETag` of
//! the last answer: almost every ask is a `304` with no body. The six hours are counted from the
//! *cache file's* age rather than from startup, so restarting a workspace ten times an afternoon
//! asks once. A failure is filed rather than said — a laptop on a train would otherwise hear about
//! GitHub every six hours — and leaves the copy in use exactly as it was.
//!
//! `models.update = false` turns it off and `models.url` points it elsewhere — a mirror inside a
//! firewall, or a file of your own in the same format.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use neosh_provider::{manifest, t3code};

/// Where t3code keeps its manifest.
pub const URL: &str = t3code::URL;

/// How often to ask.
pub const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// Not before this long after startup: the first screen has better things to do with the network
/// than a file that is, almost always, unchanged.
const SETTLE: Duration = Duration::from_secs(5);

/// Bigger than this is not a model catalogue, whatever it says it is.
const MAX_BYTES: usize = 1 << 20;

fn cache_file(dir: &Path) -> PathBuf {
    dir.join("t3code-models.json")
}

fn etag_file(dir: &Path) -> PathBuf {
    dir.join("t3code-models.etag")
}

/// Put the copy kept from the last fetch in use, if it is newer than the snapshot compiled in.
///
/// Before the providers are built, because they read the catalogue as they are made: a workspace
/// that started on the bundled list and swapped a minute later would draw last month's models on
/// the first `^P`.
pub fn load_cached(dir: &Path) {
    let Ok(text) = std::fs::read_to_string(cache_file(dir)) else { return };
    match t3code::parse(&text) {
        Ok(news) => {
            manifest::offer(news);
        }
        Err(e) => tracing::debug!("ignoring the cached model catalogue: {e}"),
    }
}

/// How long until the next ask is due, from the age of what was kept last time.
pub fn due_in(dir: Option<&Path>) -> Duration {
    let age = dir
        .and_then(|d| std::fs::metadata(cache_file(d)).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok());
    match age {
        Some(age) if age < EVERY => (EVERY - age).max(SETTLE),
        _ => SETTLE,
    }
}

/// What an ask came back with.
#[derive(Debug)]
pub enum Fetched {
    /// The same file as last time, or one no newer than the copy in use.
    Unchanged,
    /// A newer catalogue, now in use. The model ids it added and dropped, as `catalogue/id`.
    Installed { added: Vec<String>, removed: Vec<String> },
}

/// Ask `url` for t3code's manifest, and put it in use if it is newer.
pub async fn fetch(url: &str, dir: Option<&Path>) -> Result<Fetched, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("neosh/", env!("CARGO_PKG_VERSION")))
        // On a background task that must not sit forever, and short because nothing waits for it.
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(url);
    // Only when the file it describes is still there: an `ETag` without its body is a `304` for a
    // file this machine no longer has.
    if let Some(d) = dir
        && cache_file(d).exists()
        && let Ok(tag) = std::fs::read_to_string(etag_file(d))
        && !tag.trim().is_empty()
    {
        req = req.header(reqwest::header::IF_NONE_MATCH, tag.trim());
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        // Asked and answered: the clock starts again from now, which is what the file's age is.
        if let Some(d) = dir
            && let Ok(text) = std::fs::read(cache_file(d))
        {
            let _ = write_atomic(&cache_file(d), &text);
        }
        return Ok(Fetched::Unchanged);
    }
    if !resp.status().is_success() {
        return Err(format!("{url} answered {}", resp.status()));
    }
    if resp.content_length().is_some_and(|n| n as usize > MAX_BYTES) {
        return Err(format!("{url} is too large to be a model catalogue"));
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("{url} is too large to be a model catalogue"));
    }
    let text = String::from_utf8(bytes.to_vec()).map_err(|_| format!("{url} is not text"))?;
    let news = t3code::parse(&text)?;

    // Kept whether or not it is newer than the copy in use, so the next ask can be a `304`.
    if let Some(d) = dir {
        let _ = std::fs::create_dir_all(d);
        if write_atomic(&cache_file(d), text.as_bytes()).is_ok() {
            match &etag {
                Some(tag) => {
                    let _ = write_atomic(&etag_file(d), tag.as_bytes());
                }
                None => {
                    let _ = std::fs::remove_file(etag_file(d));
                }
            }
        }
    }

    let before = manifest::current().ids();
    if !manifest::offer(news) {
        return Ok(Fetched::Unchanged);
    }
    let after = manifest::current().ids();
    Ok(Fetched::Installed {
        added: after.iter().filter(|id| !before.contains(id)).cloned().collect(),
        removed: before.iter().filter(|id| !after.contains(id)).cloned().collect(),
    })
}

/// Written beside and renamed over, so a workspace killed mid-write leaves the old copy rather
/// than half of a new one.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ask_is_due_soon_with_nothing_kept_and_later_with_something_fresh() {
        let dir = std::env::temp_dir().join(format!("neosh-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(due_in(Some(&dir)), SETTLE, "nothing kept: ask soon");
        assert_eq!(due_in(None), SETTLE, "nowhere to keep it: ask soon");

        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(cache_file(&dir), "{}").expect("write");
        let wait = due_in(Some(&dir));
        assert!(wait > EVERY - Duration::from_secs(60), "just fetched: wait out the interval");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Phone app icons on disk, shared with the GUI and the fex icon download.
//!
//! Cache dir: `$XDG_CACHE_HOME/smartconnect-linux/app-icons/` (default `~/.cache/…`). Files are named
//! like the phone's svc 17 `iconName` (`<pkg>_<cmp>.png`, `<userId>_` prefixed for other users), or
//! `<pkg>.png`. This module writes the second kind: the base64 PNG the phone already sends in
//! service 15 `cmd 2` (stream invite) and `cmd 13` (focus change), so an app streamed once has its real
//! icon in the GUI even before the icon download works.

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

pub fn icon_cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
        .join("smartconnect-linux/app-icons")
}

/// A package name safe to use as a file name (it comes from the phone).
fn safe_pkg(pkg: &str) -> bool {
    !pkg.is_empty()
        && !pkg.starts_with('.')
        && pkg.len() <= 255
        && pkg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Saves `icon` (base64 PNG, optionally a `data:` URL) as `<dir>/<pkg>.png` unless that file exists.
/// Atomic: written to a temp file, then renamed. Returns the path only when a new file was written.
pub fn harvest_into(dir: &Path, pkg: &str, icon: &str) -> Option<PathBuf> {
    if !safe_pkg(pkg) {
        return None;
    }
    let dest = dir.join(format!("{pkg}.png"));
    if dest.exists() {
        return None;
    }
    let b64 = icon.split_once("base64,").map_or(icon, |(_, b)| b);
    let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = B64.decode(clean).ok()?;
    if !bytes.starts_with(PNG_MAGIC) {
        return None;
    }
    std::fs::create_dir_all(dir).ok()?;
    let tmp = dir.join(format!(".{pkg}.png.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes).ok()?;
    match std::fs::rename(&tmp, &dest) {
        Ok(()) => Some(dest),
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            None
        }
    }
}

/// [`harvest_into`] the shared cache dir.
pub fn harvest(pkg: &str, icon: &str) -> Option<PathBuf> {
    harvest_into(&icon_cache_dir(), pkg, icon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harvest_writes_once_and_rejects_bad_input() {
        let dir = std::env::temp_dir().join(format!("sc-mirror-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let png = [PNG_MAGIC, b"rest-of-image"].concat();
        let b64 = B64.encode(&png);

        let path = harvest_into(&dir, "com.example.notes", &b64).expect("first icon is written");
        assert_eq!(path, dir.join("com.example.notes.png"));
        assert_eq!(std::fs::read(&path).unwrap(), png);
        // Present already: left alone.
        assert!(
            harvest_into(
                &dir,
                "com.example.notes",
                &B64.encode([PNG_MAGIC, b"other"].concat())
            )
            .is_none()
        );
        assert_eq!(std::fs::read(&path).unwrap(), png);
        // data: URL prefix is accepted.
        assert!(
            harvest_into(
                &dir,
                "com.example.b",
                &format!("data:image/png;base64,{b64}")
            )
            .is_some()
        );
        // Not a PNG, not base64, or an unsafe package name: nothing written.
        assert!(harvest_into(&dir, "com.example.c", &B64.encode(b"GIF89a")).is_none());
        assert!(harvest_into(&dir, "com.example.d", "!!!").is_none());
        assert!(harvest_into(&dir, "../evil", &b64).is_none());
        assert!(harvest_into(&dir, "a/b", &b64).is_none());
        assert!(!dir.join("com.example.c.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

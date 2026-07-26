//! Cross-backend SVG cache used only for KiCad compatibility ordering.

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(any(feature = "renderer-kicad-cli", feature = "renderer-vello"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

#[cfg(any(feature = "renderer-kicad-cli", feature = "renderer-vello"))]
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn cache_root() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|root| root.join("konnect").join("schematic-viewer"))
}

fn stable_path_hash(path: &Path) -> u64 {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn cache_path(schematic: &Path) -> Option<PathBuf> {
    let metadata = fs::metadata(schematic).ok()?;
    let stamp = metadata
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(cache_root()?.join(format!(
        "{:016x}-{stamp:032x}-{:016x}.svg",
        stable_path_hash(schematic),
        metadata.len()
    )))
}

#[cfg(any(feature = "renderer-kicad-cli", feature = "renderer-vello"))]
pub(crate) fn store(schematic: &Path, svg: &str) -> std::io::Result<()> {
    let Some(target) = cache_path(schematic) else {
        return Ok(());
    };
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    if target.is_file() {
        return Ok(());
    }
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        target.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        sequence
    ));
    fs::write(&temporary, svg)?;
    fs::rename(&temporary, target)
}

#[cfg(feature = "renderer-vello")]
pub(crate) fn load_fresh(schematic: &Path) -> Option<String> {
    let cache = cache_path(schematic)?;
    let source_modified = fs::metadata(schematic).ok()?.modified().ok()?;
    let cache_modified = fs::metadata(&cache).ok()?.modified().ok()?;
    if cache_modified < source_modified || cache_modified == SystemTime::UNIX_EPOCH {
        return None;
    }
    fs::read_to_string(cache).ok()
}

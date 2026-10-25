//! Rendering resolved paths for people.
//!
//! `canonicalize` on Windows returns an extended-length ("verbatim") path,
//! `\\?\C:\…` or `\\?\UNC\server\share\…`. That form is right for comparing
//! two paths and wrong for a message: it sits beside the caller's own spelling
//! of the same directory, and it is not what a user types back (#673).

use std::path::Path;

/// `path` as a message should show it. Comparisons keep the path itself.
///
/// Not limited to Windows: a path resolved on Unix starts with `/`, never
/// `\\?\`, and a rule that runs everywhere is one every platform's tests
/// exercise.
pub fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    without_verbatim_prefix(&text).unwrap_or(text)
}

/// The ordinary spelling of a verbatim drive or UNC path, or `None` when
/// there is none that names the same file.
///
/// Without the prefix Windows parses a path again: it rejects one of
/// `MAX_PATH` or more, maps a reserved device name such as `NUL` to the
/// device, trims a trailing dot or space, and reads `C:` as the current
/// directory on that drive. A path any of that would change keeps its prefix.
fn without_verbatim_prefix(text: &str) -> Option<String> {
    const MAX_PATH: usize = 260;

    let (plain, components) = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        if unc.is_empty() {
            return None;
        }
        (format!(r"\\{unc}"), unc)
    } else {
        let rest = text.strip_prefix(r"\\?\")?;
        let bytes = rest.as_bytes();
        if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || &bytes[1..3] != br":\" {
            return None;
        }
        (rest.to_string(), &rest[3..])
    };
    let unchanged_when_reparsed = plain.len() < MAX_PATH
        && (components.is_empty() || components.split('\\').all(is_plain_component));
    unchanged_when_reparsed.then_some(plain)
}

fn is_plain_component(name: &str) -> bool {
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];

    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let is_device = RESERVED.contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !name.chars().any(|c| c < ' ' || r#"<>:"/|?*"#.contains(c))
        && !is_device
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verbatim_drive_path_loses_only_its_prefix() {
        assert_eq!(
            without_verbatim_prefix(r"\\?\C:\Users\me\proj\board.kicad_pcb").as_deref(),
            Some(r"C:\Users\me\proj\board.kicad_pcb")
        );
        assert_eq!(without_verbatim_prefix(r"\\?\D:\").as_deref(), Some(r"D:\"));
    }

    #[test]
    fn a_verbatim_unc_path_keeps_its_leading_separators() {
        assert_eq!(
            without_verbatim_prefix(r"\\?\UNC\server\share\proj\board.kicad_pcb").as_deref(),
            Some(r"\\server\share\proj\board.kicad_pcb")
        );
    }

    #[test]
    fn other_verbatim_forms_are_left_alone() {
        for text in [
            r"\\?\Volume{0b1e0f6a-0000-0000-0000-100000000000}\board.kicad_pcb",
            r"\\?\GLOBALROOT\Device\HarddiskVolume2\board.kicad_pcb",
            r"\\?\C:board.kicad_pcb",
            r"\\?\D:",
            r"\\?\UNC\",
            r"\\?\",
        ] {
            assert_eq!(without_verbatim_prefix(text), None, "{text}");
        }
    }

    /// Each of these names a different file, or none, once the prefix is
    /// gone, so the message keeps the form that names the right one.
    #[test]
    fn a_path_windows_would_reparse_differently_keeps_its_prefix() {
        let long = format!(r"\\?\C:\{}\board.kicad_pcb", "d".repeat(250));
        for text in [
            long.as_str(),
            r"\\?\C:\proj\nul.kicad_pcb",
            r"\\?\C:\proj\COM1\board.kicad_pcb",
            r"\\?\C:\proj.\board.kicad_pcb",
            r"\\?\C:\proj \board.kicad_pcb",
            r"\\?\C:\proj\..\board.kicad_pcb",
            r"\\?\C:\proj\a/b.kicad_pcb",
            r"\\?\UNC\server\share\aux\board.kicad_pcb",
        ] {
            assert_eq!(without_verbatim_prefix(text), None, "{text}");
        }
        assert_eq!(
            without_verbatim_prefix(r"\\?\C:\proj\console\com10.kicad_pcb").as_deref(),
            Some(r"C:\proj\console\com10.kicad_pcb")
        );
    }

    /// What a resolved path renders as must resolve back to the same file, so
    /// a caller can paste it into the next call. On Windows this is the case
    /// the issue reported: `canonicalize` adds the prefix.
    #[test]
    fn a_canonicalized_path_renders_without_the_prefix_and_resolves_back() {
        let directory = tempfile::tempdir().unwrap();
        let board_path = directory.path().join("board.kicad_pcb");
        std::fs::write(&board_path, "").unwrap();
        let canonical = board_path.canonicalize().unwrap();

        let shown = display_path(&canonical);

        assert!(!shown.starts_with(r"\\?\"), "{shown}");
        assert_eq!(Path::new(&shown).canonicalize().unwrap(), canonical);
    }

    /// A path that does not exist reaches messages through lexical
    /// normalization, not `canonicalize`, so it never had a prefix.
    #[test]
    fn plain_drive_unc_and_unix_paths_are_left_alone() {
        for text in [
            r"C:\Users\me\proj\missing.kicad_pcb",
            r"\\server\share\proj\board.kicad_pcb",
            "/home/me/proj/board.kicad_pcb",
        ] {
            assert_eq!(display_path(Path::new(text)), text);
        }
    }
}

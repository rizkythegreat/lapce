use std::path::Path;

/// Quote a local file as one shell argument, followed by a separator for the
/// next file or prompt text. Never allow terminal control characters in a drop.
pub(super) fn quote_dropped_path(
    path: &Path,
    shell: Option<&str>,
) -> Result<String, &'static str> {
    let path = path.to_str().ok_or("The file path is not valid UTF-8.")?;
    if path.chars().any(char::is_control) {
        return Err("File paths containing control characters cannot be dropped.");
    }

    let shell = shell
        .unwrap_or(if cfg!(windows) { "powershell" } else { "sh" })
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match shell.as_str() {
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => {
            Ok(format!("'{}' ", path.replace('\'', "''")))
        }
        "cmd" | "cmd.exe" => {
            if path.contains(['%', '!', '"']) {
                return Err(
                    "This file path contains characters that cmd.exe cannot paste safely.",
                );
            }
            Ok(format!("\"{path}\" "))
        }
        _ => Ok(format!("'{}' ", path.replace('\'', "'\\''"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_path_is_one_literal_argument() {
        assert_eq!(
            quote_dropped_path(
                Path::new("/tmp/gambar saya's $(touch nope).png"),
                Some("/bin/zsh"),
            )
            .unwrap(),
            "'/tmp/gambar saya'\\''s $(touch nope).png' "
        );
    }

    #[test]
    fn powershell_escapes_apostrophes() {
        assert_eq!(
            quote_dropped_path(
                Path::new("C:\\Images\\it's $photo.png"),
                Some("C:\\Windows\\powershell.exe"),
            )
            .unwrap(),
            "'C:\\Images\\it''s $photo.png' "
        );
    }

    #[test]
    fn cmd_quotes_spaces_and_rejects_expansion() {
        assert_eq!(
            quote_dropped_path(Path::new("C:\\my images\\photo.png"), Some("cmd.exe"))
                .unwrap(),
            "\"C:\\my images\\photo.png\" "
        );
        for path in ["C:\\%USERNAME%.png", "C:\\!name!.png"] {
            assert!(quote_dropped_path(Path::new(path), Some("cmd.exe")).is_err());
        }
    }

    #[test]
    fn control_characters_are_rejected() {
        for path in ["/tmp/a\nb.png", "/tmp/a\rb.png", "/tmp/\x1b[201~.png"] {
            assert!(quote_dropped_path(Path::new(path), Some("sh")).is_err());
        }
    }

    #[test]
    fn unicode_paths_are_preserved() {
        assert_eq!(
            quote_dropped_path(Path::new("/tmp/截图 café.png"), Some("bash"))
                .unwrap(),
            "'/tmp/截图 café.png' "
        );
    }
}

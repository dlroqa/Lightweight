//! Markdown paths pasted or dropped into the terminal, and profile onboarding.

use std::path::{Path, PathBuf};

use lightagent_core::paths::{create_private_dir, write_private};

const MAX_MARKDOWN_BYTES: u64 = 65_536;
const ONBOARDING_NAME: &str = "user-onboarding";
const ONBOARDING_MANIFEST: &str = r#"{"name":"user-onboarding","description":"Profile onboarding guidance","instructions_file":"ONBOARDING.md"}"#;

/// Recognize a standalone dropped Markdown path without mistaking an ordinary
/// question mentioning a `.md` file for a drop.
pub(crate) fn dropped_path(input: &str) -> Option<PathBuf> {
    let path = parse_path(input).ok()?;
    let explicit = input
        .trim()
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '/' | '.' | '~' | '\'' | '"'))
        || input.trim().starts_with("file://");
    if path.is_file() || explicit {
        Some(path)
    } else {
        None
    }
}

/// Normalize terminal quoting and shell-escaped spaces; no shell is invoked.
pub(crate) fn parse_path(input: &str) -> Result<PathBuf, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("drop a Markdown file or enter its path".to_owned());
    }
    let raw = if raw.len() >= 2
        && ((raw.starts_with('\'') && raw.ends_with('\''))
            || (raw.starts_with('"') && raw.ends_with('"')))
    {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    let file_url = raw.strip_prefix("file://");
    let raw = file_url.unwrap_or(raw);
    let url_path = if let Some(raw) = file_url {
        if let Some(path) = raw.strip_prefix("localhost/") {
            format!("/{path}")
        } else if raw.starts_with('/') {
            raw.to_owned()
        } else {
            return Err("file URL must name a local absolute path".to_owned());
        }
    } else {
        raw.to_owned()
    };
    let url_path = if file_url.is_some() {
        decode_percent(&url_path)?
    } else {
        url_path
    };
    let decoded = if cfg!(windows) {
        url_path
    } else {
        let mut decoded = String::new();
        let mut chars = url_path.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some(next) => decoded.push(next),
                    None => return Err("Markdown path ends in a backslash".to_owned()),
                }
            } else {
                decoded.push(ch);
            }
        }
        decoded
    };
    let decoded = if let Some(rest) = decoded.strip_prefix("~/") {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|home| {
                PathBuf::from(home)
                    .join(rest)
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or(decoded)
    } else {
        decoded
    };
    let path = PathBuf::from(decoded);
    let markdown = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"));
    if !markdown {
        return Err("the dropped file must end in .md or .markdown".to_owned());
    }
    Ok(path)
}

fn decode_percent(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .ok_or_else(|| "invalid percent escape in file URL".to_owned())?;
            let digits = std::str::from_utf8(hex).map_err(|e| e.to_string())?;
            let value = u8::from_str_radix(digits, 16)
                .map_err(|_| "invalid percent escape in file URL".to_owned())?;
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "file URL is not valid UTF-8".to_owned())
}

pub(crate) fn read(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| format!("could not read Markdown file {}: {e}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_MARKDOWN_BYTES {
        return Err(format!(
            "{} is too large (maximum {} KiB)",
            path.display(),
            MAX_MARKDOWN_BYTES / 1024
        ));
    }
    std::fs::read_to_string(path)
        .map_err(|e| format!("could not read Markdown file {}: {e}", path.display()))
}

/// Install or replace the active profile's onboarding bundle. The old bundle
/// stays available until the new one is fully written and can be renamed in.
pub(crate) fn install_onboarding(source: &Path, profile_dir: &Path) -> Result<(), String> {
    let body = read(source)?;
    let root = profile_dir.join("extensions");
    create_private_dir(&root).map_err(|e| e.to_string())?;
    let id = lightagent_core::RunId::new();
    let staging = profile_dir.join(format!(".onboarding-stage-{}", id.as_str()));
    let backup = profile_dir.join(format!(".onboarding-backup-{}", id.as_str()));
    let destination = root.join(ONBOARDING_NAME);
    create_private_dir(&staging).map_err(|e| e.to_string())?;
    let write_result = (|| {
        write_private(
            &staging.join("extension.json"),
            ONBOARDING_MANIFEST.as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        write_private(&staging.join("ONBOARDING.md"), body.as_bytes())
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let replacing = match std::fs::symlink_metadata(&destination) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            if std::fs::read_to_string(destination.join("extension.json"))
                .ok()
                .as_deref()
                != Some(ONBOARDING_MANIFEST)
            {
                let _ = std::fs::remove_dir_all(&staging);
                return Err("an unrelated extension already uses 'user-onboarding'".to_owned());
            }
            true
        }
        Ok(_) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err("existing onboarding bundle is not a regular directory".to_owned());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(format!("could not inspect onboarding bundle: {error}"));
        }
    };
    if replacing && let Err(error) = std::fs::rename(&destination, &backup) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("could not prepare onboarding update: {error}"));
    }
    if let Err(error) = std::fs::rename(&staging, &destination) {
        if replacing {
            let _ = std::fs::rename(&backup, &destination);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("could not install onboarding: {error}"));
    }
    if replacing {
        let _ = std::fs::remove_dir_all(&backup);
    }
    Ok(())
}

pub(crate) fn remove_onboarding(profile_dir: &Path) -> Result<(), String> {
    let root = profile_dir.join("extensions");
    let manifest = std::fs::read_to_string(root.join(ONBOARDING_NAME).join("extension.json"))
        .map_err(|e| format!("profile onboarding is not installed: {e}"))?;
    if manifest != ONBOARDING_MANIFEST {
        return Err("an unrelated extension uses 'user-onboarding'".to_owned());
    }
    lightagent_extensions::uninstall(ONBOARDING_NAME, &root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightagent_core::ExtensionsConfig;
    use lightagent_extensions::ExtensionStore;

    #[test]
    fn dropped_paths_accept_terminal_forms() {
        let dir = std::env::temp_dir().join(format!(
            "lightagent-drop-{}",
            lightagent_core::RunId::new().as_str()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("My Notes.md");
        std::fs::write(&file, "# Notes").unwrap();
        assert_eq!(
            dropped_path(&format!("'{}'", file.display())),
            Some(file.clone())
        );
        // Backslash escaping and `file://` drops are terminal conventions of
        // Unix shells. On Windows a backslash is the separator, so the path is
        // taken literally and neither form is synthesized by the terminal.
        #[cfg(unix)]
        {
            assert_eq!(
                dropped_path(&file.display().to_string().replace(' ', "\\ ")),
                Some(file.clone())
            );
            assert_eq!(
                dropped_path(&format!(
                    "file://{}",
                    file.display().to_string().replace(' ', "%20")
                )),
                Some(file.clone())
            );
        }
        #[cfg(windows)]
        assert_eq!(
            dropped_path(&file.display().to_string()),
            Some(file.clone())
        );
        assert_eq!(dropped_path("Can you read README.md?"), None);
        assert_eq!(read(&file).unwrap(), "# Notes");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn onboarding_can_be_replaced_and_removed() {
        let dir = std::env::temp_dir().join(format!(
            "lightagent-onboard-{}",
            lightagent_core::RunId::new().as_str()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.md");
        let profile = dir.join("profile");
        std::fs::write(&source, "First guidance").unwrap();
        install_onboarding(&source, &profile).unwrap();
        std::fs::write(&source, "Updated guidance").unwrap();
        install_onboarding(&source, &profile).unwrap();
        let store = ExtensionStore::load(&[profile.join("extensions")]);
        let instructions = store.instructions(&ExtensionsConfig::default());
        assert!(instructions.contains("Updated guidance"));
        assert!(!instructions.contains("First guidance"));
        remove_onboarding(&profile).unwrap();
        assert!(ExtensionStore::load(&[profile.join("extensions")]).is_empty());
        std::fs::remove_dir_all(dir).ok();
    }
}

//! `lightagent import hermes` — migrate a Hermes home into Lightagent.
//!
//! Reads `<hermes>/profiles/<name>/` (its `profile.yaml` description, `SOUL.md`
//! persona, and the `model.default`/`model.base_url` from `config.yaml`) into a
//! Lightagent profile, and copies `<hermes>/skills/<name>/SKILL.md` into the
//! Lightagent skills directory. Idempotent: an existing profile or skill is left
//! alone unless `--force`.
//!
//! Hermes manifests are YAML, and the workspace takes on no YAML dependency, so a
//! handful of fields are read by targeted line scanning rather than a full parse —
//! enough for name, description, model and base URL; the persona is copied
//! verbatim. The model routing is best-effort and worth reviewing after import.

use std::path::{Path, PathBuf};

use lightagent_core::{AgentProfile, LightagentPaths, ProfileId, ProfileStore};

/// What an import did (or would do, under `--dry-run`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub profiles: Vec<String>,
    pub skills: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

/// The CLI entry: resolve the homes and run the import.
pub fn hermes(from: Option<PathBuf>, dry_run: bool, force: bool, json: bool) -> Result<(), String> {
    let hermes_home = from
        .or_else(|| std::env::var_os("HERMES_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".hermes")))
        .ok_or_else(|| "could not locate a Hermes home; pass --from <dir>".to_owned())?;
    if !hermes_home.is_dir() {
        return Err(format!(
            "no Hermes home at {} (pass --from <dir>)",
            hermes_home.display()
        ));
    }
    let paths = LightagentPaths::resolve().map_err(|error| error.to_string())?;
    let store = ProfileStore::new(paths.root());
    let report = run_import(&hermes_home, &store, &paths.skills_dir(), dry_run, force);

    if json {
        let value = serde_json::json!({
            "dry_run": dry_run,
            "hermes_home": hermes_home.display().to_string(),
            "profiles": report.profiles,
            "skills": report.skills,
            "skipped": report.skipped,
            "errors": report.errors,
        });
        println!("{value:#}");
        return Ok(());
    }

    let verb = if dry_run { "would import" } else { "imported" };
    println!(
        "{verb} {} profile(s) and {} skill(s) from {}",
        report.profiles.len(),
        report.skills.len(),
        hermes_home.display()
    );
    if !report.profiles.is_empty() {
        println!("  profiles: {}", report.profiles.join(", "));
    }
    if !report.skills.is_empty() {
        println!("  skills:   {}", report.skills.join(", "));
    }
    if !report.skipped.is_empty() {
        println!(
            "  skipped (exists, use --force): {}",
            report.skipped.join(", ")
        );
    }
    for error in &report.errors {
        eprintln!("  · {error}");
    }
    Ok(())
}

/// The testable core: import from `hermes_home` into `store` and `skills_dst`.
fn run_import(
    hermes_home: &Path,
    store: &ProfileStore,
    skills_dst: &Path,
    dry_run: bool,
    force: bool,
) -> ImportReport {
    let mut report = ImportReport::default();
    import_profiles(hermes_home, store, dry_run, force, &mut report);
    import_skills(hermes_home, skills_dst, dry_run, force, &mut report);
    report
}

fn import_profiles(
    hermes_home: &Path,
    store: &ProfileStore,
    dry_run: bool,
    force: bool,
    report: &mut ImportReport,
) {
    let Ok(entries) = std::fs::read_dir(hermes_home.join("profiles")) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let raw = entry.file_name().to_string_lossy().into_owned();
        let id = match sanitize_profile_id(&raw) {
            Some(id) => id,
            None => {
                report
                    .errors
                    .push(format!("profile {raw:?} has no usable id"));
                continue;
            }
        };
        if store.load(&id).is_ok() && !force {
            report.skipped.push(format!("profile {raw}"));
            continue;
        }
        let persona = std::fs::read_to_string(dir.join("SOUL.md")).unwrap_or_default();
        let profile_yaml = std::fs::read_to_string(dir.join("profile.yaml")).unwrap_or_default();
        let config_yaml = std::fs::read_to_string(dir.join("config.yaml")).unwrap_or_default();
        let description = top_level_value(&profile_yaml, "description");
        let model = block_value(&config_yaml, "model", "default").unwrap_or_default();
        let base_url = block_value(&config_yaml, "model", "base_url");

        let mut profile = AgentProfile::new(id.clone(), raw.clone(), persona, model);
        profile.description = description.unwrap_or_default();
        profile.routing.base_url = base_url;
        if dry_run {
            report.profiles.push(id.as_str().to_owned());
        } else if let Err(error) = store.save(&profile) {
            report.errors.push(format!("could not save {raw}: {error}"));
        } else {
            report.profiles.push(id.as_str().to_owned());
        }
    }
}

fn import_skills(
    hermes_home: &Path,
    skills_dst: &Path,
    dry_run: bool,
    force: bool,
    report: &mut ImportReport,
) {
    let Ok(entries) = std::fs::read_dir(hermes_home.join("skills")) else {
        return;
    };
    for entry in entries.flatten() {
        let src = entry.path().join("SKILL.md");
        if !src.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let dst_dir = skills_dst.join(&name);
        let dst = dst_dir.join("SKILL.md");
        if dst.exists() && !force {
            report.skipped.push(format!("skill {name}"));
            continue;
        }
        if dry_run {
            report.skills.push(name);
            continue;
        }
        if let Err(error) =
            std::fs::create_dir_all(&dst_dir).and_then(|()| std::fs::copy(&src, &dst).map(|_| ()))
        {
            report
                .errors
                .push(format!("could not copy skill {name}: {error}"));
        } else {
            report.skills.push(name);
        }
    }
}

/// Turn an arbitrary Hermes profile name into a valid [`ProfileId`], or `None`.
fn sanitize_profile_id(raw: &str) -> Option<ProfileId> {
    let mut out = String::new();
    for ch in raw.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let out: String = out
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .chars()
        .take(64)
        .collect();
    ProfileId::new(out).ok()
}

/// Read a top-level `key:` scalar, joining folded (indented) continuation lines.
fn top_level_value(yaml: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    let mut lines = yaml.lines();
    let mut value = loop {
        let line = lines.next()?;
        if let Some(rest) = line.strip_prefix(&prefix) {
            // A top-level key has no leading indentation.
            if line.starts_with(char::is_whitespace) {
                continue;
            }
            break rest.trim().to_owned();
        }
    };
    // Fold indented continuation lines onto the value.
    for line in lines {
        if line.starts_with(char::is_whitespace) && !line.trim().is_empty() {
            value.push(' ');
            value.push_str(line.trim());
        } else {
            break;
        }
    }
    let value = unquote(value.trim());
    if value.is_empty() { None } else { Some(value) }
}

/// Read `block: { key: value }` from an indented YAML mapping block.
fn block_value(yaml: &str, block: &str, key: &str) -> Option<String> {
    let block_head = format!("{block}:");
    let key_head = format!("{key}:");
    let mut in_block = false;
    for line in yaml.lines() {
        if !in_block {
            if line.trim_end() == block_head && !line.starts_with(char::is_whitespace) {
                in_block = true;
            }
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            break; // dedent: left the block
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&key_head) {
            let value = unquote(rest.trim());
            if value.is_empty() || value.starts_with("${") {
                return None; // an unset or secret-reference value is not usable here
            }
            return Some(value);
        }
    }
    None
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if value.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_ids() {
        assert_eq!(sanitize_profile_id("Iris").unwrap().as_str(), "iris");
        assert_eq!(sanitize_profile_id("My Bot!").unwrap().as_str(), "my-bot-");
        assert!(sanitize_profile_id("!!!").is_none());
    }

    #[test]
    fn reads_nested_and_folded_yaml() {
        let config = "model:\n  provider: custom\n  default: MiniCPM5\n  base_url: http://x/v1\nagent:\n  foo: bar\n";
        assert_eq!(
            block_value(config, "model", "default").as_deref(),
            Some("MiniCPM5")
        );
        assert_eq!(
            block_value(config, "model", "base_url").as_deref(),
            Some("http://x/v1")
        );
        assert_eq!(block_value(config, "agent", "default"), None);

        let profile = "description: Line one\n  and line two.\ndescription_auto: false\n";
        assert_eq!(
            top_level_value(profile, "description").as_deref(),
            Some("Line one and line two.")
        );
    }

    #[test]
    fn a_secret_reference_value_is_not_imported() {
        let config = "model:\n  api_key: ${SOME_SECRET}\n";
        assert_eq!(block_value(config, "model", "api_key"), None);
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lightagent-import-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn imports_a_profile_and_a_skill() {
        let hermes = scratch();
        let profile_dir = hermes.join("profiles/iris");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("SOUL.md"), "You are Iris.").unwrap();
        std::fs::write(
            profile_dir.join("profile.yaml"),
            "description: A helper bot\n",
        )
        .unwrap();
        std::fs::write(
            profile_dir.join("config.yaml"),
            "model:\n  default: gpt-x\n  base_url: http://h/v1\n",
        )
        .unwrap();
        let skill_dir = hermes.join("skills/pdf");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "---\nname: pdf\n---\nbody").unwrap();

        let home = scratch();
        let store = ProfileStore::new(&home);
        let skills_dst = home.join("skills");

        let report = run_import(&hermes, &store, &skills_dst, false, false);
        assert_eq!(report.profiles, vec!["iris".to_string()]);
        assert_eq!(report.skills, vec!["pdf".to_string()]);

        let loaded = store.load(&ProfileId::new("iris").unwrap()).unwrap();
        assert_eq!(loaded.persona, "You are Iris.");
        assert_eq!(loaded.description, "A helper bot");
        assert_eq!(loaded.routing.model, "gpt-x");
        assert_eq!(loaded.routing.base_url.as_deref(), Some("http://h/v1"));
        assert!(skills_dst.join("pdf/SKILL.md").is_file());

        // Re-running without force skips both.
        let again = run_import(&hermes, &store, &skills_dst, false, false);
        assert!(again.profiles.is_empty());
        assert_eq!(again.skipped.len(), 2);

        std::fs::remove_dir_all(&hermes).ok();
        std::fs::remove_dir_all(&home).ok();
    }
}

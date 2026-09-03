//! Skills: packaged `SKILL.md` instruction sets the agent can load on demand.
//!
//! A skill is a directory holding a `SKILL.md` whose YAML-style frontmatter names
//! it and describes when to use it, with the body being the instructions. The
//! same shape Hermes and Claude Code use, so a skill written for one is readable
//! here (and the Hermes importer can copy them across).
//!
//! The runtime uses skills by *progressive disclosure*: the [`catalog`] of names
//! and descriptions is added to the system prompt so the model knows what exists,
//! and it calls the `skill.read` tool to pull one skill's full body only when it
//! is actually relevant — so many skills cost little until used.
//!
//! Frontmatter is parsed by reading the `name:`/`description:` lines between the
//! `---` fences, not a full YAML parser — the workspace takes on no YAML
//! dependency, and a skill file needs nothing richer for these two fields.

use std::collections::BTreeMap;
use std::path::Path;

/// One loaded skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// The name the model uses with `skill.read` (frontmatter `name`, else the
    /// directory name).
    pub name: String,
    /// A one-line summary of when to use the skill.
    pub description: String,
    /// The full instructions (everything after the frontmatter).
    pub body: String,
}

/// The skills discovered under a set of directories.
#[derive(Clone, Debug, Default)]
pub struct SkillStore {
    skills: BTreeMap<String, Skill>,
}

impl SkillStore {
    /// Load every `<dir>/<skill>/SKILL.md` under each directory in turn; a later
    /// directory's skill replaces an earlier one of the same name (so a profile's
    /// skills override the global set).
    pub fn load(dirs: &[std::path::PathBuf]) -> Self {
        let mut skills = BTreeMap::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(path.join("SKILL.md")) else {
                    continue;
                };
                let fallback = entry.file_name().to_string_lossy().into_owned();
                if let Some(skill) = parse_skill(&text, &fallback) {
                    skills.insert(skill.name.clone(), skill);
                }
            }
        }
        Self { skills }
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// The skill names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }

    /// Whether any skills were found.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// The number of skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// The prompt catalog: each skill's name and description, with a line telling
    /// the model to load a skill before related work. Empty when there are none.
    pub fn catalog(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "# Available skills\nBefore doing related work, call the `skill.read` tool with a \
             skill's name to load its full instructions.\n\n",
        );
        for skill in self.skills.values() {
            if skill.description.is_empty() {
                out.push_str(&format!("- {}\n", skill.name));
            } else {
                out.push_str(&format!("- {}: {}\n", skill.name, skill.description));
            }
        }
        out
    }
}

/// Parse a `SKILL.md`: frontmatter for `name`/`description`, the rest as body.
fn parse_skill(text: &str, fallback_name: &str) -> Option<Skill> {
    let (front, body) = split_frontmatter(text);
    let mut name = None;
    let mut description = None;
    for line in front.lines() {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "name" => name = Some(unquote(value.trim())),
                "description" => description = Some(unquote(value.trim())),
                _ => {}
            }
        }
    }
    let name = name
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback_name.to_owned());
    if name.is_empty() {
        return None;
    }
    Some(Skill {
        name,
        description: description.unwrap_or_default(),
        body: body.trim().to_owned(),
    })
}

/// Split leading `---`-fenced frontmatter from the body. When there is no fenced
/// frontmatter, the whole text is the body and the frontmatter is empty.
fn split_frontmatter(text: &str) -> (String, String) {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (String::new(), text.to_owned());
    }
    let mut front = String::new();
    let mut body = String::new();
    let mut in_body = false;
    for line in lines {
        if !in_body && line.trim_end() == "---" {
            in_body = true;
            continue;
        }
        let target = if in_body { &mut body } else { &mut front };
        target.push_str(line);
        target.push('\n');
    }
    if in_body {
        (front, body)
    } else {
        (String::new(), text.to_owned())
    }
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

/// The skill directories to load for a profile: the global set, then the
/// profile's own (which overrides on a name clash).
pub fn skill_dirs(home: &Path, profile_dir: &Path) -> Vec<std::path::PathBuf> {
    vec![home.join("skills"), profile_dir.join("skills")]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lightagent-skills-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, dir: &str, contents: &str) {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), contents).unwrap();
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let text = "---\nname: pdf-tools\ndescription: \"Work with PDFs\"\n---\n\nDo the thing.\n";
        let skill = parse_skill(text, "fallback").unwrap();
        assert_eq!(skill.name, "pdf-tools");
        assert_eq!(skill.description, "Work with PDFs");
        assert_eq!(skill.body, "Do the thing.");
    }

    #[test]
    fn falls_back_to_the_directory_name() {
        let text = "---\ndescription: no name here\n---\nbody";
        let skill = parse_skill(text, "dirname").unwrap();
        assert_eq!(skill.name, "dirname");
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let (front, body) = split_frontmatter("just text\nmore");
        assert!(front.is_empty());
        assert_eq!(body, "just text\nmore");
    }

    #[test]
    fn loads_and_overrides_across_dirs() {
        let global = scratch();
        let profile = scratch();
        write_skill(
            &global,
            "a",
            "---\nname: a\ndescription: global a\n---\nglobal",
        );
        write_skill(
            &global,
            "b",
            "---\nname: b\ndescription: global b\n---\nb body",
        );
        write_skill(
            &profile,
            "a",
            "---\nname: a\ndescription: profile a\n---\nprofile",
        );

        let store = SkillStore::load(&[global.clone(), profile.clone()]);
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("a").unwrap().description, "profile a");
        assert_eq!(store.get("a").unwrap().body, "profile");
        assert!(store.catalog().contains("- a: profile a"));
        assert!(store.catalog().contains("skill.read"));

        std::fs::remove_dir_all(&global).ok();
        std::fs::remove_dir_all(&profile).ok();
    }
}

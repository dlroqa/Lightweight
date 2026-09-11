//! The extensions shipped in the repository's `extensions/` directory load
//! through the same discovery a user's installed copy goes through, so a bundle
//! that stops parsing fails here rather than silently vanishing from a run.

use std::path::PathBuf;

use lightagent_core::{ExtensionsConfig, SkillStore};
use lightagent_extensions::ExtensionStore;

fn bundled_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extensions")
}

#[test]
fn harness_engineering_contributes_its_skills_and_nothing_else() {
    let store = ExtensionStore::load(&[bundled_dir()]);
    let ext = store
        .get("harness-engineering")
        .expect("the bundled harness-engineering extension loads");
    assert!(!ext.description.is_empty());
    assert!(
        ext.mcp_servers.is_empty(),
        "the bundle runs no external code"
    );
    assert!(
        ext.instructions.is_empty(),
        "the bundle adds nothing to every persona beyond its skill catalog lines"
    );

    let config = ExtensionsConfig::default();
    assert!(store.is_active("harness-engineering", &config));
    let skills = SkillStore::load(&store.skill_dirs(&config));
    for name in [
        "harness-agents-md",
        "harness-log",
        "harness-plan",
        "harness-resources",
        "harness-review",
    ] {
        let skill = skills
            .get(name)
            .unwrap_or_else(|| panic!("{name} is contributed"));
        assert!(!skill.description.is_empty(), "{name} has a description");
        assert!(!skill.body.is_empty(), "{name} has instructions");
    }
    assert_eq!(skills.len(), 5);

    // Disabling the extension withdraws every skill it contributed.
    let disabled = ExtensionsConfig {
        disabled: vec!["harness-engineering".to_owned()],
        ..ExtensionsConfig::default()
    };
    assert!(SkillStore::load(&store.skill_dirs(&disabled)).is_empty());
}

use std::process::Command;

#[test]
fn watchdog_is_unreleased_and_released_section_matches_main() {
    let text = include_str!("../CHANGELOG.md");
    let main = Command::new("git")
        .args(["show", "origin/main:CHANGELOG.md"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(main.status.success());
    let main = String::from_utf8(main.stdout).unwrap();
    fn released(text: &str) -> &str {
        text.split("## [0.4.1]")
            .nth(1)
            .unwrap()
            .split("## [0.4.0]")
            .next()
            .unwrap()
    }
    assert_eq!(released(text), released(&main));
    let unreleased = text.split("## [0.4.1]").next().unwrap();
    assert!(unreleased.contains("Root turns no longer have a 50-step limit"));
    assert!(!released(text).contains("watchdog"));
}

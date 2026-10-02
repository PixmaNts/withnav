//! The skills catalog: the one system message that tells the model which
//! skills exist. Bounded, so a large library cannot crowd out the work.

use super::Skill;

/// The most estimated tokens (bytes / 4) the message may take. Deliberately
/// larger than a minimal catalog would need: the driver's own library holds
/// about 110 skills of up to [`MAX_DESCRIPTION_CHARS`] characters each, and
/// every one that does not fit is invisible to the model.
pub const MAX_TOKENS: usize = 8192;
/// The longest description, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 256;

const FRAMING: &str = "Skills are reusable workflows kept in files. Before following a workflow \
that matches one of the skills below, load it with skill({\"name\":\"<id>\"}); loading only \
returns its instructions and does not run any script.";

/// The description on one line, cut at [`MAX_DESCRIPTION_CHARS`].
pub(super) fn flatten(description: &str) -> String {
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_DESCRIPTION_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(MAX_DESCRIPTION_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// The message for `skills`, already in display order; `None` when empty.
pub(super) fn render(skills: &[&Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let lines: Vec<String> = skills
        .iter()
        .map(|s| format!("{} — {}", s.name, flatten(&s.description)))
        .collect();
    let budget = MAX_TOKENS * 4;
    let omitted_line = |n: usize| format!("{n} additional skills omitted");
    // The largest prefix that fits together with the framing and, when
    // something is left out, the omission line.
    let fits = |k: usize| {
        let mut size = FRAMING.len() + lines[..k].iter().map(|l| l.len() + 1).sum::<usize>();
        if k < lines.len() {
            size += omitted_line(lines.len() - k).len() + 1;
        }
        size <= budget
    };
    let kept = (0..=lines.len()).rev().find(|&k| fits(k)).unwrap_or(0);
    let mut message = FRAMING.to_string();
    for line in &lines[..kept] {
        message.push('\n');
        message.push_str(line);
    }
    if kept < lines.len() {
        message.push('\n');
        message.push_str(&omitted_line(lines.len() - kept));
    }
    Some(message)
}

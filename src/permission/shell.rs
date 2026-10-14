//! Splitting a shell command line into commands and their arity prefixes.

/// A literal redirection, including any explicit file descriptor in `op`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirection {
    pub op: String,
    pub target: String,
}

/// One command and the source information needed by rules and the fixed floor.
/// An opaque command matches no allow or ask rule. Only a deny rule with resource
/// exactly `*` applies to it; the decision layer enforces this contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellCommand {
    pub raw: String,
    pub normalized: String,
    /// Rule matching text: identifying prefix followed by the remaining operands.
    pub rule_text: Option<String>,
    /// Unquoted words, including leading assignments and literal path operands.
    pub words: Vec<String>,
    pub prefix: Vec<String>,
    pub redirections: Vec<Redirection>,
    pub opaque: bool,
}

impl ShellCommand {
    pub fn save_pattern(&self) -> Option<String> {
        if self.opaque || self.prefix.is_empty() {
            None
        } else {
            Some(format!("{} *", self.prefix.join(" ")))
        }
    }
}

/// Sorted by identifying prefix; longer matching entries override shorter ones.
pub const ARITY: &[(&str, usize)] = &[
    ("brew", 2),
    ("bun", 2),
    ("bun run", 3),
    ("cargo", 2),
    ("cat", 1),
    ("cd", 1),
    ("chmod", 1),
    ("chown", 1),
    ("cp", 1),
    ("docker", 2),
    ("docker compose", 3),
    ("echo", 1),
    ("env", 1),
    ("export", 1),
    ("gh", 2),
    ("gh issue", 3),
    ("gh pr", 3),
    ("git", 2),
    ("git config", 3),
    ("git remote", 3),
    ("git stash", 3),
    ("glab", 2),
    ("go", 2),
    ("grep", 1),
    ("just", 2),
    ("kill", 1),
    ("kubectl", 2),
    ("kubectl config", 3),
    ("ln", 1),
    ("ls", 1),
    ("make", 2),
    ("mkdir", 1),
    ("mv", 1),
    ("npm", 2),
    ("npm run", 3),
    ("pip", 2),
    ("pnpm", 2),
    ("pnpm run", 3),
    ("ps", 1),
    ("pwd", 1),
    ("rm", 1),
    ("rmdir", 1),
    ("sleep", 1),
    ("source", 1),
    ("systemctl", 2),
    ("tail", 1),
    ("touch", 1),
    ("uv", 2),
    ("which", 1),
    ("yarn", 2),
    ("yarn run", 3),
];

fn assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn wrapper(word: &str) -> bool {
    matches!(
        basename(word),
        "sudo" | "env" | "time" | "nice" | "nohup" | "command" | "exec"
    )
}

fn wrapper_option_value(program: &str, option: &str) -> bool {
    match basename(program) {
        "sudo" => matches!(
            option,
            "-u" | "-g"
                | "-h"
                | "-p"
                | "-C"
                | "-T"
                | "-R"
                | "-D"
                | "--user"
                | "--group"
                | "--host"
                | "--prompt"
                | "--chdir"
                | "--chroot"
                | "--close-from"
                | "--command-timeout"
        ),
        "env" => matches!(
            option,
            "-u" | "-C" | "-S" | "--unset" | "--chdir" | "--split-string"
        ),
        "time" => matches!(option, "-f" | "-o" | "--format" | "--output"),
        "nice" => matches!(option, "-n" | "--adjustment"),
        "xargs" => matches!(
            option,
            "-a" | "-d"
                | "-E"
                | "-I"
                | "-L"
                | "-n"
                | "-P"
                | "-s"
                | "--arg-file"
                | "--delimiter"
                | "--eof"
                | "--replace"
                | "--max-lines"
                | "--max-args"
                | "--max-procs"
                | "--max-chars"
        ),
        "exec" => option == "-a",
        _ => false,
    }
}

fn execution_bearing(program: &str, operands: &[String]) -> bool {
    let program = basename(program);
    matches!(program, "eval" | "xargs" | "parallel" | "watch")
        || (program == "env"
            && operands.iter().any(|word| {
                word == "-S"
                    || word.starts_with("-S")
                    || word == "--split-string"
                    || word.starts_with("--split-string=")
            }))
        || (matches!(
            program,
            "exec" | "bash" | "sh" | "zsh" | "dash" | "ksh" | "fish"
        ) && operands
            .iter()
            .any(|word| word.starts_with('-') && !word.starts_with("--") && word.contains('c')))
        || (program == "find"
            && operands
                .iter()
                .any(|word| matches!(word.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")))
}

fn command_identity(words: &[String], plain: &[bool]) -> Result<(Vec<String>, Option<String>), ()> {
    if plain.first() == Some(&false) {
        return Err(());
    }
    let mut index = 0;
    let mut selected = Vec::new();
    let mut assignments = vec![false; words.len()];
    while words
        .get(index)
        .is_some_and(|word| plain[index] && assignment(word))
    {
        assignments[index] = true;
        index += 1;
    }
    while let Some(program) = words.get(index).filter(|word| wrapper(word)) {
        if execution_bearing(program, &words[index + 1..]) {
            return Err(());
        }
        selected.push(index);
        index += 1;
        while let Some(word) = words.get(index) {
            if word == "--" {
                index += 1;
                break;
            }
            if (plain[index] && assignment(word)) || word.starts_with('-') {
                // Only explicitly understood options may be skipped. Clusters,
                // attached values and unknown options can change execution.
                if word.starts_with('-') && !wrapper_option_value(program, word) {
                    return Err(());
                }
                if wrapper_option_value(program, word) && index + 1 >= words.len() {
                    return Err(());
                }
                assignments[index] = plain[index] && assignment(word);
                index += 1;
                if wrapper_option_value(program, word) && index < words.len() {
                    index += 1;
                }
            } else {
                break;
            }
        }
    }
    if let Some(program) = words.get(index) {
        if execution_bearing(program, &words[index + 1..]) {
            return Err(());
        }
        let mut significant = vec![index];
        index += 1;
        while let Some(word) = words.get(index) {
            index += 1;
            let git_global =
                program == "git" && significant.len() == 1 && matches!(word.as_str(), "-C" | "-c");
            let compose_file = program == "docker"
                && significant.len() == 2
                && words[significant[1]] == "compose"
                && matches!(word.as_str(), "-f" | "--file");
            if git_global || compose_file {
                index = (index + 1).min(words.len());
            } else if !word.starts_with('-') {
                significant.push(index - 1);
            }
        }
        let count = ARITY
            .iter()
            .filter(|(key, _)| {
                key.split(' ')
                    .zip(&significant)
                    .all(|(a, b)| a == words[*b])
                    && key.split(' ').count() <= significant.len()
            })
            .max_by_key(|(key, _)| key.len())
            .map_or(1, |(_, count)| *count);
        selected.extend(significant.into_iter().take(count));
    }
    if selected.iter().any(|&i| !plain[i]) {
        return Err(());
    }
    let prefix: Vec<String> = selected.iter().map(|&i| words[i].clone()).collect();
    let rule_text = if prefix.is_empty() {
        None
    } else {
        let mut reordered = prefix.clone();
        reordered.extend(
            words
                .iter()
                .enumerate()
                .filter(|(i, _)| !assignments[*i] && !selected.contains(i))
                .map(|(_, word)| word.clone()),
        );
        Some(reordered.join(" "))
    };
    Ok((prefix, rule_text))
}

// Keep a source boundary map so lexical continuation removal does not lose raw
// spelling. Single-quoted text and comments retain their literal newlines.
fn lexical_text(line: &str) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut offsets = Vec::new();
    let mut quote = None;
    let mut comment = false;
    let mut boundary = true;
    let mut pos = 0;
    while pos < line.len() {
        let rest = &line[pos..];
        let c = rest.chars().next().expect("nonempty remainder");
        if !comment && quote != Some('\'') && rest.starts_with("\\\n") {
            pos += 2;
            continue;
        }
        let mut length = c.len_utf8();
        if comment {
            if c == '\n' {
                comment = false;
            }
        } else if c == '\\' && quote != Some('\'') {
            if let Some(next) = rest[1..].chars().next() {
                length += next.len_utf8();
            }
        } else if Some(c) == quote {
            quote = None;
        } else if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if quote.is_none() && c == '#' && boundary {
            comment = true;
        }
        text.push_str(&line[pos..pos + length]);
        offsets.extend(pos..pos + length);
        pos += length;
        boundary = quote.is_none() && (c.is_whitespace() || matches!(c, ';' | '|' | '&' | '('));
    }
    offsets.push(line.len());
    offsets[0] = 0;
    (text, offsets)
}

/// Split supported shell syntax without executing or expanding anything.
/// Unsupported or malformed syntax is retained as one opaque command. Opaque
/// commands have no inferred words or prefix and must not be generalized.
/// Outer commands precede their substitutions; raw slices retain surrounding
/// whitespace but exclude command separators and grouping delimiters.
pub fn split(line: &str) -> Vec<ShellCommand> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    let (text, offsets) = lexical_text(line);
    let mut parser = Parser {
        text: &text,
        source: line,
        offsets: &offsets,
        pos: 0,
        backticks: 0,
    };
    match parser.list(None, 0) {
        Ok(commands) if !commands.is_empty() => commands,
        _ => vec![ShellCommand {
            raw: line.to_owned(),
            normalized: line.to_owned(),
            rule_text: None,
            words: Vec::new(),
            prefix: Vec::new(),
            redirections: Vec::new(),
            opaque: true,
        }],
    }
}

struct Parser<'a> {
    text: &'a str,
    source: &'a str,
    offsets: &'a [usize],
    pos: usize,
    backticks: usize,
}

#[derive(Default)]
struct Pending {
    words: Vec<String>,
    plain: Vec<bool>,
    redirections: Vec<Redirection>,
    children: Vec<ShellCommand>,
}

impl Pending {
    fn finish(&mut self, raw: &str, commands: &mut Vec<ShellCommand>) -> Result<(), ()> {
        // Compound control syntax needs a grammar rather than guessed commands.
        if self.words.first().is_some_and(|word| {
            matches!(
                word.as_str(),
                "if" | "then"
                    | "else"
                    | "elif"
                    | "fi"
                    | "for"
                    | "while"
                    | "until"
                    | "do"
                    | "done"
                    | "case"
                    | "esac"
                    | "function"
                    | "coproc"
                    | "select"
                    | "[["
                    | "]]"
            )
        }) {
            return Err(());
        }
        if !self.words.is_empty() || !self.redirections.is_empty() {
            let normalized = self
                .words
                .iter()
                .enumerate()
                .skip_while(|(i, word)| self.plain[*i] && assignment(word))
                .map(|(_, word)| word.clone())
                .collect::<Vec<_>>()
                .join(" ");
            let (prefix, rule_text) = command_identity(&self.words, &self.plain)?;
            self.plain.clear();
            commands.push(ShellCommand {
                raw: raw.to_owned(),
                normalized,
                prefix,
                rule_text,
                words: std::mem::take(&mut self.words),
                redirections: std::mem::take(&mut self.redirections),
                opaque: false,
            });
        }
        commands.append(&mut self.children);
        Ok(())
    }
}

impl Parser<'_> {
    fn raw(&self, start: usize) -> &str {
        &self.source[self.offsets[start]..self.offsets[self.pos]]
    }
    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn advance(&mut self) -> Result<char, ()> {
        let c = self.peek().ok_or(())?;
        self.pos += c.len_utf8();
        Ok(c)
    }

    fn list(&mut self, stop: Option<char>, depth: usize) -> Result<Vec<ShellCommand>, ()> {
        // Bound nested shell syntax to keep arbitrary input off the Rust stack.
        if depth > 64 {
            return Err(());
        }
        let mut commands = Vec::new();
        let mut pending = Pending::default();
        let mut start = self.pos;
        let mut needs_command = false;
        let mut has_command = false;
        while let Some(c) = self.peek() {
            if Some(c) == stop {
                if needs_command {
                    return Err(());
                }
                pending.finish(self.raw(start), &mut commands)?;
                self.advance()?;
                return Ok(commands);
            }
            if c.is_whitespace() && c != '\n' {
                self.advance()?;
            } else if c == '#' {
                pending.finish(self.raw(start), &mut commands)?;
                while self.peek().is_some_and(|c| c != '\n') {
                    self.advance()?;
                }
                start = self.pos;
            } else if let Some(op) = self.redirection_op() {
                self.pos += op.len();
                if op.contains("<<") && !op.ends_with("<<<") {
                    return Err(());
                }
                while self.peek().is_some_and(|c| c.is_whitespace() && c != '\n') {
                    self.advance()?;
                }
                let (target, _) = self.word(&mut pending.children, depth, stop)?;
                pending.redirections.push(Redirection { op, target });
                needs_command = false;
                has_command = true;
            } else if matches!(c, '|' | '&' | ';' | '\n') {
                if c != '\n' && !has_command {
                    return Err(());
                }
                pending.finish(self.raw(start), &mut commands)?;
                self.advance()?;
                let mut binary = c == '|';
                if (c == '|' && matches!(self.peek(), Some('|' | '&')))
                    || (c == '&' && self.peek() == Some('&'))
                {
                    self.advance()?;
                    binary = true;
                }
                if c != '\n' {
                    needs_command = binary;
                }
                has_command = false;
                start = self.pos;
            } else if c == '(' || (c == '{' && self.rest()[1..].starts_with(char::is_whitespace)) {
                if !pending.words.is_empty() {
                    return Err(());
                }
                if self.rest().starts_with("((") {
                    return Err(());
                }
                pending.finish(self.raw(start), &mut commands)?;
                self.advance()?;
                commands.extend(self.list(Some(if c == '(' { ')' } else { '}' }), depth + 1)?);
                needs_command = false;
                has_command = true;
                start = self.pos;
            } else if matches!(c, ')' | '}') {
                return Err(());
            } else {
                let word_start = self.pos;
                let (word, plain) = self.word(&mut pending.children, depth, stop)?;
                if &self.text[word_start..self.pos] == "!" && pending.words.is_empty() {
                    needs_command = true;
                    continue;
                }
                pending.words.push(word);
                pending.plain.push(plain);
                needs_command = false;
                has_command = true;
            }
        }
        if stop.is_some() || needs_command {
            return Err(());
        }
        pending.finish(self.raw(start), &mut commands)?;
        Ok(commands)
    }

    fn redirection_op(&self) -> Option<String> {
        if self.rest().starts_with("<(") || self.rest().starts_with(">(") {
            return None;
        }
        let digits = self.rest().bytes().take_while(u8::is_ascii_digit).count();
        let rest = &self.rest()[digits..];
        [
            "&>>", "<<<", "<<-", ">>", "<<", ">&", "<&", "<>", ">|", "&>", ">", "<",
        ]
        .into_iter()
        .find(|op| rest.starts_with(op))
        .map(|op| self.rest()[..digits + op.len()].to_owned())
    }

    fn substitution(
        &mut self,
        children: &mut Vec<ShellCommand>,
        depth: usize,
    ) -> Result<String, ()> {
        let start = self.pos;
        let backtick = self.peek() == Some('`');
        self.pos += if backtick { 1 } else { 2 };
        if !backtick && self.peek() == Some('(') {
            return Err(());
        }
        self.backticks += usize::from(backtick);
        let nested = self.list(Some(if backtick { '`' } else { ')' }), depth + 1);
        self.backticks -= usize::from(backtick);
        children.extend(nested?);
        Ok(self.text[start..self.pos].to_owned())
    }

    /// One word, and whether it is plain: no quote or escape outside a command
    /// substitution, and a substitution only in the value of an assignment.
    /// Quoting inside `$(…)` belongs to the inner command, which is split and
    /// judged on its own; it does not make `S=$(…)` ambiguous. A substitution
    /// anywhere else leaves the word non-plain, so it never names a program
    /// or a rule prefix.
    fn word(
        &mut self,
        children: &mut Vec<ShellCommand>,
        depth: usize,
        stop: Option<char>,
    ) -> Result<(String, bool), ()> {
        let start = self.pos;
        let mut value = String::new();
        let mut quote = None;
        let (mut quoted, mut substituted) = (false, false);
        while let Some(c) = self.peek() {
            if self.backticks > 0 && self.rest().starts_with("\\`") {
                // Backticks undergo an extra escape-processing pass in Bash.
                // Retain the entire line instead of hiding nested execution.
                return Err(());
            }
            if quote.is_none() && Some(c) == stop {
                break;
            } else if quote == Some('\'') {
                quoted = true;
                self.advance()?;
                if c == '\'' {
                    quote = None;
                } else {
                    value.push(c);
                }
            } else if Some(c) == quote {
                self.advance()?;
                quote = None;
            } else if c == '\\' {
                quoted = true;
                self.advance()?;
                let escaped = self.advance()?;
                if escaped != '\n' {
                    if quote == Some('"') && !matches!(escaped, '$' | '`' | '"' | '\\') {
                        value.push('\\');
                    }
                    value.push(escaped);
                }
            } else if self.rest().starts_with("$(")
                || c == '`'
                || (quote.is_none()
                    && (self.rest().starts_with("<(") || self.rest().starts_with(">(")))
            {
                substituted = true;
                value.push_str(&self.substitution(children, depth)?);
            } else if quote.is_none()
                && (self.rest().starts_with("$'") || self.rest().starts_with("$\""))
            {
                // ANSI-C and locale quoting cannot be normalized by quote removal.
                return Err(());
            } else if quote.is_none() && matches!(c, '\'' | '"') {
                quoted = true;
                self.advance()?;
                quote = Some(c);
            } else if quote.is_none()
                && (c.is_whitespace() || matches!(c, '|' | '&' | ';' | '(' | ')' | '<' | '>'))
            {
                break;
            } else {
                value.push(self.advance()?);
            }
        }
        if quote.is_some() || self.pos == start {
            return Err(());
        }
        let plain = !quoted
            && (!substituted
                || value
                    .split_once('=')
                    .is_some_and(|(name, _)| assignment(&format!("{name}="))));
        Ok((value, plain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn command_prefix_table() {
        for (line, expected) in [
            ("git push origin main", vec!["git push"]),
            ("git -C ../x status", vec!["git status"]),
            ("cargo run --bin nav", vec!["cargo run"]),
            ("cargo test -p withnav", vec!["cargo test"]),
            ("docker compose up -d", vec!["docker compose up"]),
            ("npm run build", vec!["npm run build"]),
            ("FOO=1 make check > out.log 2>&1", vec!["make check"]),
            ("cat a | grep \"x;y\" && rm -rf build", vec!["cat", "grep", "rm"]),
            ("echo $(git rev-parse HEAD)", vec!["echo", "git rev-parse"]),
            ("sudo systemctl restart nginx", vec!["sudo systemctl restart"]),
            ("./script.sh a", vec!["./script.sh"]),
            ("git -c core.editor=vim -C ../x config get user.name", vec!["git config get"]),
            ("sudo -u root env -u OLD FOO=1 time -o timing nice -n 5 nohup command -- exec -a nav git status", vec!["sudo env time nice nohup command exec git status"]),
            ("(git status; cargo test) |& { echo ok; pwd; } & ls\nwhoami || true", vec!["git status", "cargo test", "echo", "pwd", "ls", "whoami", "true"]),
            ("cat <(git show HEAD) >(grep x)", vec!["cat", "git show", "grep"]),
            ("echo `git status`", vec!["echo", "git status"]),
            ("echo \"$(git status)\" '$(not executed)'", vec!["echo", "git status"]),
            ("echo a\\;b \\| c\\&d", vec!["echo"]),
            ("echo $(git status && cargo test) `pwd`", vec!["echo", "git status", "cargo test", "pwd"]),
            ("git push &&\n git status # trailing comment", vec!["git push", "git status"]),
        ] {
            let commands = split(line);
            assert!(commands.iter().all(|command| !command.opaque), "{line}");
            assert_eq!(commands.iter().map(|command| command.prefix.join(" ")).collect::<Vec<_>>(), expected, "{line}");
        }
    }

    #[test]
    fn keeps_source_words_and_redirections() {
        let line = "  FOO=1 make   check > out.log 2>&1  ";
        let commands = split(line);
        let command = &commands[0];
        assert_eq!(command.raw, line);
        assert_eq!(command.normalized, "make check");
        assert_eq!(command.words, ["FOO=1", "make", "check"]);
        assert_eq!(
            command.redirections,
            [
                Redirection {
                    op: ">".into(),
                    target: "out.log".into()
                },
                Redirection {
                    op: "2>&".into(),
                    target: "1".into()
                },
            ]
        );
        assert_eq!(command.save_pattern().as_deref(), Some("make check *"));
        let quoted = split("FOO=ab echo \"literal path\" a\\ b 'x;y' \\\nnext");
        assert_eq!(quoted[0].normalized, "echo literal path a b x;y next");
        assert_eq!(
            quoted[0].words,
            ["FOO=ab", "echo", "literal path", "a b", "x;y", "next"]
        );
        let commands = split("cat < 'literal path' &>>log <<<\"text\"");
        assert_eq!(commands[0].normalized, "cat");
        assert_eq!(
            commands[0]
                .redirections
                .iter()
                .map(|r| (r.op.as_str(), r.target.as_str()))
                .collect::<Vec<_>>(),
            [("<", "literal path"), ("&>>", "log"), ("<<<", "text")]
        );
    }

    #[test]
    fn malformed_or_unsupported_lines_are_wholly_opaque() {
        for line in [
            "echo \"unbalanced",
            "git status; echo 'bad",
            "cat <<EOF\ntext\nEOF",
            "echo $(git status",
            "(git status",
            "git status)",
            "echo \\",
            "echo >",
            "if true; then pwd; fi",
            "echo $((1 + 2))",
            "# comment",
            ";;;",
            "git status &&",
            "git status |",
            "| git status",
            "git status && ; pwd",
            "echo $'escaped\\n'",
            "function_name() { pwd; }",
        ] {
            let commands = split(line);
            assert_eq!(commands.len(), 1, "{line}");
            assert!(commands[0].opaque, "{line}");
            assert_eq!(commands[0].raw, line);
        }
        assert!(split(" \t\n").is_empty());
        let nested = format!("echo {}pwd{}", "$(".repeat(1000), ")".repeat(1000));
        assert!(split(&nested)[0].opaque);
    }

    #[test]
    fn arity_table_is_sorted_and_complete() {
        assert!(ARITY.windows(2).all(|entries| entries[0].0 < entries[1].0));
        assert_eq!(ARITY.len(), 51);
        for &(key, count) in ARITY {
            if key == "env" {
                continue;
            }
            let line = format!("{key} --flag one two three four");
            let commands = split(&line);
            assert_eq!(commands[0].prefix.len(), count, "{key}");
        }
        assert_eq!(split("unknown --flag arg")[0].prefix, ["unknown"]);
        assert_eq!(split("env")[0].prefix, ["env"]);
    }

    proptest! {
        #[test]
        fn arbitrary_lines_never_panic_or_lose_nonblank_input(line in any::<String>()) {
            let commands = split(&line);
            prop_assert!(line.trim().is_empty() || !commands.is_empty());
            if commands.iter().any(|command| command.opaque) {
                prop_assert_eq!(commands.len(), 1);
                prop_assert_eq!(&commands[0].raw, &line);
            }
        }
    }

    #[test]
    fn wrapper_options_and_execution_paths_are_conservative() {
        for line in [
            "env -iSrm git push",
            "env -iu X git push",
            "sudo -iu root git push",
            "nice -n5 git push",
            "time -vo f git push",
            "nohup -xy git push",
            "command -pv git push",
            "exec -la name git push",
            "env --unknown git push",
            "sudo --unknown git push",
            "nice --unknown git push",
            "time --unknown git push",
            "nohup --unknown git push",
            "command --unknown git push",
            "exec --unknown git push",
            "/bin/bash -c 'rm victim'",
            "/usr/bin/xargs rm",
            "/usr/bin/find . -exec rm {} \\;",
            "/usr/bin/env -S 'rm' x",
            "./eval 'rm victim'",
            "/usr/bin/env -iSrm git push",
            "/usr/bin/sudo -iu root git push",
            "./parallel rm",
            "./watch rm",
            "/bin/exec -c 'rm victim'",
            "env -u",
            "/usr/bin/sudo /bin/sh -c 'rm victim'",
        ] {
            let commands = split(line);
            assert_eq!(commands.len(), 1, "{line}");
            assert!(commands[0].opaque, "{line}");
            assert_eq!(commands[0].rule_text, None);
            assert_eq!(commands[0].save_pattern(), None);
        }
        let command = &split("/usr/bin/sudo -u root /usr/bin/env -u X git push origin")[0];
        assert!(!command.opaque);
        assert_eq!(
            command.prefix,
            ["/usr/bin/sudo", "/usr/bin/env", "git", "push"]
        );
        assert_eq!(
            command.rule_text.as_deref(),
            Some("/usr/bin/sudo /usr/bin/env git push -u root -u X origin")
        );
    }

    #[test]
    fn ambiguous_identity_and_embedded_execution_are_opaque() {
        for line in [
            "'git push' origin",
            "\"git\" push",
            "'FOO=1' git push",
            "g\\it push",
            "git 'push' origin",
            "git p\\ush origin",
            "FOO='a b' git push",
            "env -S 'rm -f' git push",
            "env --split-string 'rm -f' git push",
            "env --split-string=rm git push",
            "eval 'git push; rm victim'",
            "exec -c 'rm victim'",
            "bash -c 'rm victim'",
            "sh -c 'rm victim'",
            "zsh -xc 'rm victim'",
            "dash -c 'rm victim'",
            "ksh -c 'rm victim'",
            "fish -c 'rm victim'",
            "sudo bash -lc 'rm victim'",
            "find . -exec rm {} \\;",
            "find . -execdir rm {} +",
            "find . -ok rm {} \\;",
            "find . -okdir rm {} \\;",
            "xargs -I CMD CMD",
            "xargs rm",
            "xargs",
            "parallel rm",
            "watch rm victim",
        ] {
            let commands = split(line);
            assert_eq!(commands.len(), 1, "{line}");
            assert!(commands[0].opaque, "{line}");
            assert_eq!(commands[0].raw, line);
            assert_eq!(commands[0].rule_text, None);
            assert_eq!(commands[0].save_pattern(), None);
        }
        for line in [
            "source script.sh",
            ". script.sh",
            "timeout 5 git status",
            "echo 'git push'",
            "FOO=1 git push",
        ] {
            assert!(!split(line)[0].opaque, "{line}");
        }
    }

    #[test]
    fn reviewed_syntax_never_hides_execution() {
        for line in [
            "echo `echo \\`printf NESTED\\``",
            "coproc git status",
            "select x in a; do pwd; done",
            "if true; then pwd; fi",
            "while true; do pwd; done",
            "until true; do pwd; done",
            "for x in a; do pwd; done",
            "case x in x) pwd;; esac",
            "function f { pwd; }",
            "[[ -f x ]]",
            "(( 1 + 2 ))",
        ] {
            let commands = split(line);
            assert_eq!(commands.len(), 1, "{line}");
            assert!(commands[0].opaque, "{line}");
            assert_eq!(commands[0].rule_text, None);
            assert_eq!(commands[0].save_pattern(), None);
        }
        let commands = split("! git status | ! grep x");
        assert_eq!(commands[0].prefix, ["git", "status"]);
        assert_eq!(commands[1].prefix, ["grep"]);
        for line in ["FOO=1", ">out", "echo 'unclosed"] {
            assert_eq!(split(line)[0].save_pattern(), None);
        }
    }

    #[test]
    fn saved_patterns_match_reordered_rule_text_and_bare_prefixes() {
        // Permission matching treats a trailing space-star as optional operands.
        fn matches(pattern: &str, text: &str) -> bool {
            let prefix = pattern.strip_suffix(" *").unwrap();
            text == prefix
                || text
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with(' '))
        }
        for (line, expected) in [
            (
                "git -C ../x push origin main",
                "git push -C ../x origin main",
            ),
            (
                "git -c core.editor=vim status",
                "git status -c core.editor=vim",
            ),
            (
                "sudo -u root systemctl restart nginx",
                "sudo systemctl restart -u root nginx",
            ),
            (
                "docker compose -f a.yml up -d",
                "docker compose up -f a.yml -d",
            ),
            ("env -u X FOO=1 git status", "env git status -u X"),
            (
                "time -o f nice -n 5 git status",
                "time nice git status -o f -n 5",
            ),
            ("FOO=1 make check >out", "make check"),
            ("git status", "git status"),
        ] {
            let command = &split(line)[0];
            assert_eq!(command.rule_text.as_deref(), Some(expected), "{line}");
            let pattern = command.save_pattern().unwrap();
            assert!(matches(&pattern, expected));
            assert!(matches(&pattern, &command.prefix.join(" ")));
        }
        assert!(!matches("git status *", "git status-other"));
    }

    #[test]
    fn an_assigned_substitution_keeps_the_line_understood() {
        let commands = split("S=$(sqlite3 -readonly a.db \"SELECT 1\"); echo \"$S\"");
        assert!(commands.iter().all(|command| !command.opaque));
        assert_eq!(
            commands
                .iter()
                .map(|command| command.prefix.join(" "))
                .collect::<Vec<_>>(),
            ["", "sqlite3", "echo"]
        );
        assert_eq!(commands[0].save_pattern(), None);
        // A substitution that could name the program stays opaque.
        for line in ["$(echo rm) -rf x", "`echo rm` x", "S=1 $(echo rm) x"] {
            assert!(split(line)[0].opaque, "{line}");
        }
    }

    #[test]
    fn continuations_are_removed_before_tokens_but_not_literals_or_comments() {
        let line = "\\\necho 2\\\n>out";
        let command = &split(line)[0];
        assert_eq!(command.prefix, ["echo"]);
        assert_eq!(command.words, ["echo"]);
        assert_eq!(
            command.redirections,
            [Redirection {
                op: "2>".into(),
                target: "out".into()
            }]
        );
        assert_eq!(command.raw, line);
        assert_eq!(split("echo '' \\\n x")[0].words, ["echo", "", "x"]);
        assert_eq!(split("echo 'a\\\nb'")[0].words, ["echo", "a\\\nb"]);
        let commands = split("echo ok # comment\\\npwd");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[1].prefix, ["pwd"]);
    }
}

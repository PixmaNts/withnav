//! Splitting a shell command line into commands and their arity prefixes.

/// A literal redirection, including any explicit file descriptor in `op`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirection {
    pub op: String,
    pub target: String,
}

/// One command and the source information needed by rules and the fixed floor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellCommand {
    pub raw: String,
    pub normalized: String,
    /// Unquoted words, including leading assignments and literal path operands.
    pub words: Vec<String>,
    pub prefix: Vec<String>,
    pub redirections: Vec<Redirection>,
    pub opaque: bool,
}

impl ShellCommand {
    pub fn save_pattern(&self) -> String {
        format!("{} *", self.prefix.join(" "))
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

fn wrapper(word: &str) -> bool {
    matches!(
        word,
        "sudo" | "env" | "time" | "nice" | "nohup" | "xargs" | "command" | "exec"
    )
}

fn wrapper_option_value(program: &str, option: &str) -> bool {
    match program {
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

fn arity_prefix(words: &[String]) -> Vec<String> {
    let mut index = 0;
    let mut prefix = Vec::new();
    while words.get(index).is_some_and(|word| assignment(word)) {
        index += 1;
    }
    while let Some(program) = words.get(index).filter(|word| wrapper(word)) {
        prefix.push(program.clone());
        index += 1;
        while let Some(word) = words.get(index) {
            if word == "--" {
                index += 1;
                break;
            }
            if assignment(word) || word.starts_with('-') {
                index += 1;
                if wrapper_option_value(program, word) && index < words.len() {
                    index += 1;
                }
            } else {
                break;
            }
        }
    }
    let Some(program) = words.get(index) else {
        return prefix;
    };
    let mut significant = vec![program.clone()];
    index += 1;
    while let Some(word) = words.get(index) {
        index += 1;
        if program == "git" && significant.len() == 1 && matches!(word.as_str(), "-C" | "-c") {
            index = (index + 1).min(words.len());
        } else if !word.starts_with('-') {
            significant.push(word.clone());
        }
    }
    let count = ARITY
        .iter()
        .filter(|(key, _)| {
            key.split(' ').zip(&significant).all(|(a, b)| a == b)
                && key.split(' ').count() <= significant.len()
        })
        .max_by_key(|(key, _)| key.len())
        .map_or(1, |(_, count)| *count);
    prefix.extend(significant.into_iter().take(count));
    prefix
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
    let mut parser = Parser { text: line, pos: 0 };
    match parser.list(None, 0) {
        Ok(commands) if !commands.is_empty() => commands,
        _ => vec![ShellCommand {
            raw: line.to_owned(),
            normalized: line.to_owned(),
            words: Vec::new(),
            prefix: Vec::new(),
            redirections: Vec::new(),
            opaque: true,
        }],
    }
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

#[derive(Default)]
struct Pending {
    words: Vec<String>,
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
                .skip_while(|word| assignment(word))
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            commands.push(ShellCommand {
                raw: raw.to_owned(),
                normalized,
                prefix: arity_prefix(&self.words),
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
                pending.finish(&self.text[start..self.pos], &mut commands)?;
                self.advance()?;
                return Ok(commands);
            }
            if c.is_whitespace() && c != '\n' {
                self.advance()?;
            } else if c == '#' {
                pending.finish(&self.text[start..self.pos], &mut commands)?;
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
                let target = self.word(&mut pending.children, depth, stop)?;
                pending.redirections.push(Redirection { op, target });
                needs_command = false;
                has_command = true;
            } else if matches!(c, '|' | '&' | ';' | '\n') {
                if c != '\n' && !has_command {
                    return Err(());
                }
                pending.finish(&self.text[start..self.pos], &mut commands)?;
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
                pending.finish(&self.text[start..self.pos], &mut commands)?;
                self.advance()?;
                commands.extend(self.list(Some(if c == '(' { ')' } else { '}' }), depth + 1)?);
                needs_command = false;
                has_command = true;
                start = self.pos;
            } else if matches!(c, ')' | '}') {
                return Err(());
            } else {
                pending
                    .words
                    .push(self.word(&mut pending.children, depth, stop)?);
                needs_command = false;
                has_command = true;
            }
        }
        if stop.is_some() || needs_command {
            return Err(());
        }
        pending.finish(&self.text[start..self.pos], &mut commands)?;
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
        children.extend(self.list(Some(if backtick { '`' } else { ')' }), depth + 1)?);
        Ok(self.text[start..self.pos].to_owned())
    }

    fn word(
        &mut self,
        children: &mut Vec<ShellCommand>,
        depth: usize,
        stop: Option<char>,
    ) -> Result<String, ()> {
        let start = self.pos;
        let mut value = String::new();
        let mut quote = None;
        while let Some(c) = self.peek() {
            if quote.is_none() && Some(c) == stop {
                break;
            } else if quote == Some('\'') {
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
                value.push_str(&self.substitution(children, depth)?);
            } else if quote.is_none()
                && (self.rest().starts_with("$'") || self.rest().starts_with("$\""))
            {
                // ANSI-C and locale quoting cannot be normalized by quote removal.
                return Err(());
            } else if quote.is_none() && matches!(c, '\'' | '"') {
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
        Ok(value)
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
            ("sudo -u root env -u OLD FOO=1 time -o timing nice -n 5 nohup xargs -n 1 command -- exec -a nav git status", vec!["sudo env time nice nohup xargs command exec git status"]),
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
        let line = "  FOO=1 make   'check' > out.log 2>&1  ";
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
        assert_eq!(command.save_pattern(), "make check *");
        let quoted = split("FOO='a b' echo \"literal path\" a\\ b 'x;y' \\\nnext");
        assert_eq!(quoted[0].normalized, "echo literal path a b x;y next");
        assert_eq!(
            quoted[0].words,
            ["FOO=a b", "echo", "literal path", "a b", "x;y", "next"]
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
}

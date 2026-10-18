//! Terminal-control stripping for untrusted model and tool text.
pub fn strip_controls(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for x in chars.by_ref() {
                        if ('@'..='~').contains(&x) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(x) = chars.next() {
                        if x == '\x07' {
                            break;
                        }
                        if x == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
        } else if c == '\n' || c == '\t' || !c.is_control() {
            out.push(c)
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_control_sequences() {
        assert_eq!(strip_controls("a\x1b[31mred\x1b[0m\x07\n\tb"), "ared\n\tb");
    }
}

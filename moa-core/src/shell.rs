//! Shared shell parsing helpers used by policy normalization and matching.

/// Splits a shell command string into normalized sub-commands around chain operators.
pub fn split_shell_chain(command: &str) -> Vec<String> {
    let mut sub_commands = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape_next = false;

    while let Some(ch) = chars.next() {
        if escape_next {
            current.push(ch);
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if !in_single_quote => {
                current.push(ch);
                escape_next = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
                current.push(ch);
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
                current.push(ch);
            }
            '&' if !in_single_quote && !in_double_quote && matches!(chars.peek(), Some('&')) => {
                chars.next();
                push_sub_command(&mut sub_commands, &mut current);
            }
            '|' if !in_single_quote && !in_double_quote => {
                if matches!(chars.peek(), Some('|')) {
                    chars.next();
                }
                push_sub_command(&mut sub_commands, &mut current);
            }
            ';' if !in_single_quote && !in_double_quote => {
                push_sub_command(&mut sub_commands, &mut current);
            }
            _ => current.push(ch),
        }
    }

    push_sub_command(&mut sub_commands, &mut current);

    sub_commands
}

fn push_sub_command(sub_commands: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if trimmed.is_empty() {
        current.clear();
        return;
    }

    let normalized = shell_words::split(trimmed)
        .map(|tokens| tokens.join(" "))
        .unwrap_or_else(|_| trimmed.to_string());
    sub_commands.push(normalized);
    current.clear();
}

#[cfg(test)]
mod tests {
    use super::split_shell_chain;

    #[test]
    fn split_shell_chain_breaks_on_supported_operators() {
        assert_eq!(
            split_shell_chain("npm test && cargo fmt; rg foo | sort"),
            vec![
                "npm test".to_string(),
                "cargo fmt".to_string(),
                "rg foo".to_string(),
                "sort".to_string(),
            ]
        );
    }

    #[test]
    fn split_shell_chain_falls_back_to_trimmed_input_on_parse_failure() {
        assert_eq!(
            split_shell_chain(r#"zsh -lc "unterminated"#),
            vec![r#"zsh -lc "unterminated"#.to_string()]
        );
    }
}

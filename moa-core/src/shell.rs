//! Shared shell parsing helpers used by policy normalization and matching.

/// Splits a shell command string into normalized sub-commands around chain operators.
pub fn split_shell_chain(command: &str) -> Vec<String> {
    let Ok(tokens) = shell_words::split(command) else {
        let trimmed = command.trim();
        return if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![trimmed.to_string()]
        };
    };

    let mut sub_commands = Vec::new();
    let mut current = Vec::new();
    for token in tokens {
        if matches!(token.as_str(), "&&" | "||" | ";" | "|") {
            if !current.is_empty() {
                sub_commands.push(current.join(" "));
                current.clear();
            }
            continue;
        }
        current.push(token);
    }
    if !current.is_empty() {
        sub_commands.push(current.join(" "));
    }

    sub_commands
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

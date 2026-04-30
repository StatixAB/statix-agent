pub(super) fn container_name(attempt_id: &str) -> String {
    let suffix = attempt_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    let suffix = if suffix.is_empty() {
        "job".to_string()
    } else {
        suffix
    };
    format!("statix-{}", truncate_for_name(&suffix, 56))
}

fn truncate_for_name(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub(super) fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|value| shell_escape(value))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "@%_-+=:,./".contains(character))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn truncate_for_log(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let mut shortened = value.chars().take(max_chars).collect::<String>();
    shortened.push_str("...");
    shortened
}

use super::*;

pub(super) fn parse_cli_with_default_mode(raw_args: &[OsString]) -> Cli {
    if let Some(message) = unknown_command_error(raw_args) {
        eprintln!("{message}");
        std::process::exit(2);
    }

    Cli::parse_from(cli_args_with_default_mode(raw_args))
}

pub(super) fn unknown_command_error(raw_args: &[OsString]) -> Option<String> {
    if contains_global_help_or_version_flag(raw_args) {
        return None;
    }

    let command = first_positional_token(raw_args)?;
    if KNOWN_COMMANDS.contains(&command.as_str()) {
        return None;
    }

    let known_commands = KNOWN_COMMANDS.join(", ");
    let suggestion = find_similar_command(&command);

    Some(match suggestion {
        Some(similar) => {
            format!(
                "unknown command {command}. Did you mean {similar}? Known commands: {known_commands}"
            )
        }
        None => {
            format!("unknown command {command}. Use one of the known commands: {known_commands}")
        }
    })
}

pub(super) fn first_positional_token(raw_args: &[OsString]) -> Option<String> {
    first_positional_index(raw_args).and_then(|idx| {
        raw_args
            .get(idx)
            .map(|value| value.to_string_lossy().into_owned())
    })
}

pub(super) fn find_similar_command(input: &str) -> Option<&'static str> {
    let input_lower = input.to_ascii_lowercase();

    KNOWN_COMMANDS
        .iter()
        .copied()
        .map(|candidate| {
            let distance = levenshtein_distance(&input_lower, candidate);
            (candidate, distance)
        })
        .min_by_key(|(_, distance)| *distance)
        .and_then(|(candidate, distance)| {
            let max_len = input_lower.chars().count().max(candidate.chars().count());
            let threshold = match max_len {
                0..=4 => 1,
                5..=8 => 2,
                _ => 3,
            };

            if distance <= threshold {
                Some(candidate)
            } else {
                None
            }
        })
}

pub(super) fn levenshtein_distance(left: &str, right: &str) -> usize {
    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();

    if left_chars.is_empty() {
        return right_chars.len();
    }
    if right_chars.is_empty() {
        return left_chars.len();
    }

    let mut previous_row: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current_row = vec![0; right_chars.len() + 1];

    for (i, left_char) in left_chars.iter().enumerate() {
        current_row[0] = i + 1;

        for (j, right_char) in right_chars.iter().enumerate() {
            let substitution_cost = if left_char == right_char { 0 } else { 1 };
            let delete_cost = previous_row[j + 1] + 1;
            let insert_cost = current_row[j] + 1;
            let substitute_cost = previous_row[j] + substitution_cost;

            current_row[j + 1] = delete_cost.min(insert_cost).min(substitute_cost);
        }

        std::mem::swap(&mut previous_row, &mut current_row);
    }

    previous_row[right_chars.len()]
}

pub(super) fn cli_args_with_default_mode(raw_args: &[OsString]) -> Vec<OsString> {
    let mut args = Vec::with_capacity(raw_args.len() + 2);
    args.push(OsString::from("openusage-cli"));

    if should_insert_default_query_mode(raw_args) {
        args.push(OsString::from("query"));
    }

    args.extend(raw_args.iter().cloned());
    args
}

pub(super) fn should_insert_default_query_mode(raw_args: &[OsString]) -> bool {
    if raw_args.is_empty() {
        return true;
    }

    if contains_global_help_or_version_flag(raw_args) {
        return false;
    }

    !raw_args_contains_positional(raw_args)
}

pub(super) fn contains_global_help_or_version_flag(raw_args: &[OsString]) -> bool {
    raw_args.iter().any(|arg| {
        matches!(
            arg.to_string_lossy().as_ref(),
            "--help" | "-h" | "--version" | "-V"
        )
    })
}

pub(super) fn raw_args_contains_positional(raw_args: &[OsString]) -> bool {
    first_positional_index(raw_args).is_some()
}

fn first_positional_index(raw_args: &[OsString]) -> Option<usize> {
    let mut index = 0;
    while index < raw_args.len() {
        let token = raw_args[index].to_string_lossy();

        if token == "--" {
            return raw_args.get(index + 1).map(|_| index + 1);
        }

        if !token.starts_with('-') {
            return Some(index);
        }

        if option_requires_separate_value(&token) && !token.contains('=') {
            index += 1;
        }

        if option_optionally_consumes_separate_value(&token)
            && !token.contains('=')
            && raw_args
                .get(index + 1)
                .map(|value| is_explicit_bool_value(&value.to_string_lossy()))
                .unwrap_or(false)
        {
            index += 1;
        }

        index += 1;
    }

    None
}

pub(super) fn option_requires_separate_value(option: &str) -> bool {
    let option_name = option.split('=').next().unwrap_or(option);
    matches!(
        option_name,
        "--host"
            | "--port"
            | "--plugins-dir"
            | "--enabled-plugins"
            | "--app-data-dir"
            | "--plugin-overrides-dir"
            | "--refresh-interval-secs"
            | "--existing-instance"
            | "--service-mode"
            | "--log-level"
            | "--type"
    )
}

pub(super) fn option_optionally_consumes_separate_value(option: &str) -> bool {
    let option_name = option.split('=').next().unwrap_or(option);
    matches!(option_name, "--foreground" | "--with-state")
}

pub(super) fn is_explicit_bool_value(value: &str) -> bool {
    matches!(value, "true" | "false")
}

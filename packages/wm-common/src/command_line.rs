use std::fmt::{Display, Formatter};

/// Error returned when a command string cannot be represented as a
/// complete Windows-style argument vector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandLineParseError {
  UnterminatedQuote,
}

impl Display for CommandLineParseError {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::UnterminatedQuote => {
        formatter.write_str("command contains an unterminated quote")
      }
    }
  }
}

impl std::error::Error for CommandLineParseError {}

/// Parses a Windows command-line string into its argument vector.
///
/// Backslashes immediately before a quote follow the Windows C-runtime
/// rules: pairs become literal backslashes, while an odd trailing
/// backslash escapes the quote. Empty quoted arguments are preserved.
/// Unlike the permissive Windows runtime parser, an unterminated quote is
/// rejected so malformed configuration or IPC input cannot reach command
/// dispatch.
pub fn parse_windows_command_line(
  input: &str,
) -> Result<Vec<String>, CommandLineParseError> {
  let characters = input.chars().collect::<Vec<_>>();
  let mut arguments = Vec::new();
  let mut index = 0;

  while index < characters.len() {
    while index < characters.len() && is_separator(characters[index]) {
      index += 1;
    }
    if index == characters.len() {
      break;
    }

    let mut argument = String::new();
    let mut in_quotes = false;

    while index < characters.len() {
      let character = characters[index];
      if !in_quotes && is_separator(character) {
        break;
      }

      if character == '\\' {
        let slash_start = index;
        while index < characters.len() && characters[index] == '\\' {
          index += 1;
        }
        let slash_count = index - slash_start;

        if index < characters.len() && characters[index] == '"' {
          argument.extend(std::iter::repeat_n('\\', slash_count / 2));
          if slash_count % 2 == 0 {
            in_quotes = !in_quotes;
          } else {
            argument.push('"');
          }
          index += 1;
        } else {
          argument.extend(std::iter::repeat_n('\\', slash_count));
        }
        continue;
      }

      if character == '"' {
        in_quotes = !in_quotes;
        index += 1;
        continue;
      }

      argument.push(character);
      index += 1;
    }

    if in_quotes {
      return Err(CommandLineParseError::UnterminatedQuote);
    }

    arguments.push(argument);
  }

  Ok(arguments)
}

/// Serializes an argument vector using Windows C-runtime quoting rules.
/// Parsing the returned string with [`parse_windows_command_line`]
/// reproduces the original vector exactly.
#[must_use]
pub fn join_windows_command_line(arguments: &[String]) -> String {
  arguments
    .iter()
    .map(|argument| quote_windows_argument(argument))
    .collect::<Vec<_>>()
    .join(" ")
}

fn quote_windows_argument(argument: &str) -> String {
  if !argument.is_empty()
    && !argument
      .chars()
      .any(|character| is_separator(character) || character == '"')
  {
    return argument.to_string();
  }

  let mut quoted = String::with_capacity(argument.len() + 2);
  quoted.push('"');
  let characters = argument.chars().collect::<Vec<_>>();
  let mut index = 0;

  while index < characters.len() {
    if characters[index] != '\\' {
      if characters[index] == '"' {
        quoted.push('\\');
      }
      quoted.push(characters[index]);
      index += 1;
      continue;
    }

    let slash_start = index;
    while index < characters.len() && characters[index] == '\\' {
      index += 1;
    }
    let slash_count = index - slash_start;
    let followed_by_quote =
      index < characters.len() && characters[index] == '"';
    let output_slashes = if followed_by_quote {
      slash_count * 2 + 1
    } else if index == characters.len() {
      slash_count * 2
    } else {
      slash_count
    };
    quoted.extend(std::iter::repeat_n('\\', output_slashes));

    if followed_by_quote {
      quoted.push('"');
      index += 1;
    }
  }

  quoted.push('"');
  quoted
}

const fn is_separator(character: char) -> bool {
  matches!(character, ' ' | '\t')
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
  }

  #[test]
  fn parses_windows_quotes_backslashes_and_empty_arguments() {
    let parsed = parse_windows_command_line(
      r#"shell-exec "C:\Program Files\App\app.exe" "say \"hello\"" "" "C:\path with space\\""#,
    )
    .expect("valid Windows command line should parse");

    assert_eq!(
      parsed,
      strings(&[
        "shell-exec",
        r"C:\Program Files\App\app.exe",
        r#"say "hello""#,
        "",
        "C:\\path with space\\",
      ])
    );
  }

  #[test]
  fn preserves_unicode_and_shell_metacharacters_as_literals() {
    let arguments = strings(&[
      "shell-exec",
      "pwsh.exe",
      "日本語 dữ liệu",
      "&",
      "|",
      "<redirect>",
      "%USERPROFILE%",
      "$(not-a-shell)",
    ]);
    let serialized = join_windows_command_line(&arguments);

    assert_eq!(
      parse_windows_command_line(&serialized)
        .expect("serialized arguments should parse"),
      arguments
    );
  }

  #[test]
  fn serializer_round_trips_windows_argument_boundaries() {
    let cases = [
      strings(&["plain", "arguments"]),
      strings(&["", "two words", "trailing\\", "embedded\"quote"]),
      strings(&[r"C:\Program Files\App\", r#"a\\\"b"#, ""]),
      strings(&["tabs\tare\tpreserved", "😀", "a&b|c"]),
    ];

    for arguments in cases {
      let serialized = join_windows_command_line(&arguments);
      let reparsed = parse_windows_command_line(&serialized)
        .expect("serialized arguments should always be parseable");
      assert_eq!(reparsed, arguments, "serialized: {serialized}");
    }
  }

  #[test]
  fn rejects_unterminated_quotes() {
    assert_eq!(
      parse_windows_command_line(r#"shell-exec "unterminated"#),
      Err(CommandLineParseError::UnterminatedQuote)
    );
  }
}

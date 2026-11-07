//! Command-line dotenv file loading without changing process-global environment.
//! Later files override earlier files within each file class. Real process
//! environment variables always take precedence when the config is resolved.
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub fn load_files(
    required: &[PathBuf],
    optional: &[PathBuf],
) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let mut values = HashMap::new();
    for (path, must_exist) in required
        .iter()
        .map(|path| (path, true))
        .chain(optional.iter().map(|path| (path, false)))
    {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !must_exist => continue,
            Err(error) => {
                return Err(format!(
                    "could not load environment file {}: {error}",
                    path.display()
                )
                .into());
            }
        };
        parse_file(path, &contents, &mut values)?;
    }
    Ok(values)
}

fn parse_file(
    path: &Path,
    contents: &str,
    values: &mut HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    for (index, line) in contents.trim_start_matches('\u{feff}').lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, raw)) = line.split_once('=') else {
            return Err(format!("{}:{}: expected KEY=VALUE", path.display(), index + 1).into());
        };
        let key = key.trim();
        if !valid_key(key) {
            return Err(format!(
                "{}:{}: invalid environment variable name",
                path.display(),
                index + 1
            )
            .into());
        }
        let value =
            parse_value(raw).map_err(|why| format!("{}:{}: {why}", path.display(), index + 1))?;
        values.insert(key.to_owned(), value);
    }
    Ok(())
}

fn valid_key(key: &str) -> bool {
    let mut characters = key.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn parse_value(raw: &str) -> Result<String, &'static str> {
    let input = raw.trim_start();
    let quote = match input.chars().next() {
        Some('\'') => '\'',
        Some('"') => '"',
        _ => {
            let mut end = input.len();
            for (offset, ch) in input.char_indices() {
                if ch == '#'
                    && (offset == 0
                        || input[..offset]
                            .chars()
                            .next_back()
                            .is_some_and(char::is_whitespace))
                {
                    end = offset;
                    break;
                }
            }
            return Ok(input[..end].trim_end().to_owned());
        }
    };
    let mut parsed = String::new();
    let mut escaped = false;
    let mut closed_at = None;
    for (offset, ch) in input[1..].char_indices() {
        if quote == '"' && escaped {
            parsed.push(match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '"' => '"',
                _ => return Err("unsupported escape in double-quoted value"),
            });
            escaped = false;
        } else if quote == '"' && ch == '\\' {
            escaped = true;
        } else if ch == quote {
            closed_at = Some(offset + 2);
            break;
        } else {
            parsed.push(ch);
        }
    }
    let end = closed_at.ok_or("unterminated quoted value")?;
    let remainder = input[end..].trim();
    if !remainder.is_empty() && !remainder.starts_with('#') {
        return Err("unexpected text after quoted value");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_quotes_and_precedence() {
        let mut values = HashMap::new();
        parse_file(
            Path::new(".env"),
            "export A=' spaced # text '\nB=\"hello\\nworld\" # comment\nC=bare # comment\n",
            &mut values,
        )
        .unwrap();
        assert_eq!(values["A"], " spaced # text ");
        assert_eq!(values["B"], "hello\nworld");
        assert_eq!(values["C"], "bare");
        parse_file(Path::new(".env.local"), "C=overridden\n", &mut values).unwrap();
        assert_eq!(values["C"], "overridden");
    }

    #[test]
    fn rejects_malformed_lines_without_exposing_secrets() {
        let mut values = HashMap::new();
        for contents in [
            "NO_EQUALS",
            "BAD-NAME=abc",
            "KEY=\"unfinished",
            "KEY=\"text\" trailing",
        ] {
            assert!(parse_file(Path::new(".env"), contents, &mut values).is_err());
        }
    }

    #[test]
    fn required_missing_file_fails_and_optional_missing_file_does_not() {
        let missing =
            std::env::temp_dir().join(format!("remedia-missing-env-{}.env", std::process::id()));
        let _ = fs::remove_file(&missing);
        assert!(load_files(std::slice::from_ref(&missing), &[]).is_err());
        assert!(load_files(&[], &[missing]).unwrap().is_empty());
    }
}

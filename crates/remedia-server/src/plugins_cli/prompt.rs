//! Terminal interaction without a UI framework.
use std::io::{self, BufRead, IsTerminal, Write};

use super::Failure;

/// Whether prompts can be answered. `REMEDIA_FORCE_INTERACTIVE=1|0` overrides detection so
/// automated tests can drive prompts through a pipe.
pub fn interactive() -> bool {
    match std::env::var("REMEDIA_FORCE_INTERACTIVE").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => io::stdin().is_terminal() && io::stdout().is_terminal(),
    }
}

fn read_line() -> Result<String, Failure> {
    let mut line = String::new();

    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|error| Failure::new(format!("could not read your answer: {error}")))?;

    Ok(line.trim().to_owned())
}

/// Ask a yes/no question. The default is no; end of input is no.
pub fn confirm(question: &str) -> Result<bool, Failure> {
    print!("{question} [y/N] ");
    io::stdout().flush().ok();

    Ok(matches!(
        read_line()?.to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Ask for a comma-separated list of 1-based numbers, `all`, or nothing.
pub fn choose(question: &str, count: usize) -> Result<Vec<usize>, Failure> {
    print!("{question} ");
    io::stdout().flush().ok();

    parse_choice(&read_line()?, count)
}

fn parse_choice(answer: &str, count: usize) -> Result<Vec<usize>, Failure> {
    let answer = answer.trim();

    if answer.is_empty() || answer.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }

    if answer.eq_ignore_ascii_case("all") {
        return Ok((0..count).collect());
    }

    let mut chosen = Vec::new();

    for part in answer.split(',') {
        let number: usize = part.trim().parse().map_err(|_| {
            Failure::new(format!("`{}` is not a number from the list", part.trim()))
        })?;

        if number == 0 || number > count {
            return Err(Failure::new(format!(
                "{number} is not in the list (1-{count})"
            )));
        }

        if !chosen.contains(&(number - 1)) {
            chosen.push(number - 1);
        }
    }

    chosen.sort_unstable();

    Ok(chosen)
}

/// Print rows as an aligned table. Cells must not contain secrets.
pub fn table(headers: &[&str], rows: &[Vec<String>]) {
    let widths: Vec<usize> = (0..headers.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .chain(std::iter::once(headers[column].chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        let formatted: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();

        println!("{}", formatted.join("  ").trim_end());
    };

    line(headers.to_vec());

    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_accept_numbers_all_and_nothing() {
        assert_eq!(parse_choice("1, 3", 3).unwrap(), [0, 2]);
        assert_eq!(parse_choice("3,1,3", 3).unwrap(), [0, 2]);
        assert_eq!(parse_choice("all", 2).unwrap(), [0, 1]);
        assert!(parse_choice("", 2).unwrap().is_empty());
        assert!(parse_choice("none", 2).unwrap().is_empty());
        assert!(parse_choice("0", 2).is_err());
        assert!(parse_choice("3", 2).is_err());
        assert!(parse_choice("x", 2).is_err());
    }
}

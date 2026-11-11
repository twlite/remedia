//! Minimally destructive editing of `remedia.config.json`.
//!
//! Edits splice text into the original document instead of re-serializing it, so unrelated
//! content, key order, indentation, line endings and the trailing newline are preserved
//! byte for byte. Only the `pluginHosts` array (or one entry in it) ever changes. Callers
//! must validate the resulting document with [`crate::parse`] before committing it, and commit
//! it with [`write_atomic`], which refuses to overwrite a file changed by someone else.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

/// Failures while locating or changing configuration text.
#[derive(Debug, Error)]
pub enum EditError {
    #[error("configuration is not valid JSON: {0}")]
    Syntax(String),
    #[error("configuration `pluginHosts` must be an array")]
    NotAnArray,
    #[error("no plugin host with ID `{0}` is configured")]
    HostNotFound(String),
    #[error("plugin host `{0}` is already configured")]
    HostExists(String),
    #[error("could not access `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "`{0}` changed while it was being edited; no changes were written. Run the command again"
    )]
    Concurrent(PathBuf),
}

/// Element spans of an array together with the positions of its brackets.
type ArraySpan = (usize, Vec<(usize, usize)>, usize);

/// A JSON value with stable key order, used to render new entries.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Str(String),
    Bool(bool),
    Int(u64),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn str(value: impl Into<String>) -> Self {
        Self::Str(value.into())
    }

    pub fn object<const N: usize>(members: [(&str, Json); N]) -> Self {
        Self::Object(
            members
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }
}

#[derive(Clone, Debug)]
enum Style {
    Compact,
    Pretty {
        indent: String,
        newline: &'static str,
    },
}

impl Style {
    fn detect(source: &str) -> Self {
        if !source.contains('\n') {
            return Self::Compact;
        }

        let newline = if source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        // The first indented line that starts a JSON key defines the indentation unit.
        let indent = source
            .lines()
            .find_map(|line| {
                let trimmed = line.trim_start();

                (trimmed.starts_with('"') && trimmed.len() != line.len())
                    .then(|| line[..line.len() - trimmed.len()].to_owned())
            })
            .unwrap_or_else(|| "  ".to_owned());

        Self::Pretty { indent, newline }
    }

    fn render(&self, value: &Json, depth: usize) -> String {
        match value {
            Json::Str(text) => serde_json::to_string(text).unwrap_or_default(),
            Json::Bool(flag) => flag.to_string(),
            Json::Int(number) => number.to_string(),
            Json::Array(items) if items.is_empty() => "[]".to_owned(),
            Json::Object(members) if members.is_empty() => "{}".to_owned(),
            Json::Array(items) => self.block(
                '[',
                ']',
                depth,
                items.iter().map(|item| self.render(item, depth + 1)),
            ),
            Json::Object(members) => self.block(
                '{',
                '}',
                depth,
                members.iter().map(|(key, item)| {
                    let key = serde_json::to_string(key).unwrap_or_default();
                    let separator = if matches!(self, Self::Compact) {
                        ":"
                    } else {
                        ": "
                    };

                    format!("{key}{separator}{}", self.render(item, depth + 1))
                }),
            ),
        }
    }

    fn block(
        &self,
        open: char,
        close: char,
        depth: usize,
        items: impl Iterator<Item = String>,
    ) -> String {
        let items: Vec<String> = items.collect();

        match self {
            Self::Compact => format!("{open}{}{close}", items.join(",")),
            Self::Pretty { indent, newline } => {
                let inner = indent.repeat(depth + 1);
                let outer = indent.repeat(depth);
                let body = items
                    .iter()
                    .map(|item| format!("{inner}{item}"))
                    .collect::<Vec<_>>()
                    .join(&format!(",{newline}"));

                format!("{open}{newline}{body}{newline}{outer}{close}")
            }
        }
    }

    /// Separator placed between two members or elements at `depth`.
    fn separator(&self, depth: usize) -> String {
        match self {
            Self::Compact => ",".to_owned(),
            Self::Pretty { indent, newline } => format!(",{newline}{}", indent.repeat(depth)),
        }
    }
}

struct Scanner<'a> {
    source: &'a str,
}

struct Member {
    key: String,
    value_start: usize,
    value_end: usize,
}

impl<'a> Scanner<'a> {
    fn bytes(&self) -> &'a [u8] {
        self.source.as_bytes()
    }

    fn skip_space(&self, mut position: usize) -> usize {
        while self
            .bytes()
            .get(position)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            position += 1;
        }

        position
    }

    fn syntax(&self, position: usize) -> EditError {
        EditError::Syntax(format!("unexpected content at byte {position}"))
    }

    /// End (exclusive) of the value starting at `start`.
    fn value_end(&self, start: usize) -> Result<usize, EditError> {
        let bytes = self.bytes();

        match bytes.get(start).copied() {
            Some(b'"') => self.string_end(start),
            Some(b'{') => Ok(self.members(start)?.1 + 1),
            Some(b'[') => Ok(self.elements(start)?.1 + 1),
            Some(_) => {
                let mut end = start;

                while bytes.get(end).is_some_and(|byte| {
                    !matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
                }) {
                    end += 1;
                }

                if end == start {
                    return Err(self.syntax(start));
                }

                Ok(end)
            }
            None => Err(self.syntax(start)),
        }
    }

    fn string_end(&self, start: usize) -> Result<usize, EditError> {
        let bytes = self.bytes();
        let mut position = start + 1;

        while let Some(byte) = bytes.get(position).copied() {
            match byte {
                b'\\' => position += 2,
                b'"' => return Ok(position + 1),
                _ => position += 1,
            }
        }

        Err(self.syntax(start))
    }

    /// Members of the object opening at `open`, and the position of its closing brace.
    fn members(&self, open: usize) -> Result<(Vec<Member>, usize), EditError> {
        let bytes = self.bytes();
        let mut members = Vec::new();
        let mut position = self.skip_space(open + 1);

        if bytes.get(position) == Some(&b'}') {
            return Ok((members, position));
        }

        loop {
            if bytes.get(position) != Some(&b'"') {
                return Err(self.syntax(position));
            }

            let key_end = self.string_end(position)?;
            let key: String = serde_json::from_str(&self.source[position..key_end])
                .map_err(|error| EditError::Syntax(error.to_string()))?;

            position = self.skip_space(key_end);

            if bytes.get(position) != Some(&b':') {
                return Err(self.syntax(position));
            }

            let value_start = self.skip_space(position + 1);
            let value_end = self.value_end(value_start)?;

            members.push(Member {
                key,
                value_start,
                value_end,
            });
            position = self.skip_space(value_end);

            match bytes.get(position) {
                Some(b',') => position = self.skip_space(position + 1),
                Some(b'}') => return Ok((members, position)),
                _ => return Err(self.syntax(position)),
            }
        }
    }

    /// Element spans of the array opening at `open`, and the position of its closing bracket.
    fn elements(&self, open: usize) -> Result<(Vec<(usize, usize)>, usize), EditError> {
        let bytes = self.bytes();
        let mut elements = Vec::new();
        let mut position = self.skip_space(open + 1);

        if bytes.get(position) == Some(&b']') {
            return Ok((elements, position));
        }

        loop {
            let end = self.value_end(position)?;

            elements.push((position, end));
            position = self.skip_space(end);

            match bytes.get(position) {
                Some(b',') => position = self.skip_space(position + 1),
                Some(b']') => return Ok((elements, position)),
                _ => return Err(self.syntax(position)),
            }
        }
    }
}

/// An editable configuration document.
pub struct ConfigText {
    source: String,
    style: Style,
}

impl ConfigText {
    /// Wrap `source`, which must be a JSON object. Fails on malformed JSON.
    pub fn new(source: impl Into<String>) -> Result<Self, EditError> {
        let source = source.into();
        let value: Value =
            serde_json::from_str(&source).map_err(|error| EditError::Syntax(error.to_string()))?;

        if !value.is_object() {
            return Err(EditError::Syntax(
                "the document is not an object".to_owned(),
            ));
        }

        Ok(Self {
            style: Style::detect(&source),
            source,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// An independent copy to edit.
    pub fn clone_text(&self) -> Result<Self, EditError> {
        Self::new(self.source.clone())
    }

    pub fn into_string(self) -> String {
        self.source
    }

    fn scanner(&self) -> Scanner<'_> {
        Scanner {
            source: &self.source,
        }
    }

    fn root(&self) -> Result<(usize, Vec<Member>, usize), EditError> {
        let scanner = self.scanner();
        let open = scanner.skip_space(0);
        let (members, close) = scanner.members(open)?;

        Ok((open, members, close))
    }

    /// Span of the `pluginHosts` array, when present.
    fn hosts_array(&self) -> Result<Option<ArraySpan>, EditError> {
        let (_, members, _) = self.root()?;
        let Some(member) = members.iter().find(|member| member.key == "pluginHosts") else {
            return Ok(None);
        };

        if self.source.as_bytes()[member.value_start] != b'[' {
            return Err(EditError::NotAnArray);
        }

        let (elements, close) = self.scanner().elements(member.value_start)?;

        Ok(Some((member.value_start, elements, close)))
    }

    /// Whether `host_id` is registered.
    pub fn contains_host(&self, host_id: &str) -> Result<bool, EditError> {
        Ok(self.find_host(host_id)?.is_some())
    }

    fn find_host(&self, host_id: &str) -> Result<Option<(usize, usize)>, EditError> {
        let Some((_, elements, _)) = self.hosts_array()? else {
            return Ok(None);
        };

        for (start, end) in elements {
            let value: Value = serde_json::from_str(&self.source[start..end])
                .map_err(|error| EditError::Syntax(error.to_string()))?;

            if value.get("id").and_then(Value::as_str) == Some(host_id) {
                return Ok(Some((start, end)));
            }
        }

        Ok(None)
    }

    /// Append a host entry, creating `pluginHosts` when it does not exist.
    pub fn add_host(&self, entry: &Json) -> Result<Self, EditError> {
        let id = match entry {
            Json::Object(members) => members.iter().find_map(|(key, value)| match value {
                Json::Str(id) if key == "id" => Some(id.clone()),
                _ => None,
            }),
            _ => None,
        };

        if let Some(id) = &id
            && self.find_host(id)?.is_some()
        {
            return Err(EditError::HostExists(id.clone()));
        }

        let style = &self.style;
        let mut edited = self.source.clone();

        if let Some((open, elements, close)) = self.hosts_array()? {
            let rendered = style.render(entry, 2);

            match elements.last() {
                Some((_, last_end)) => {
                    edited.insert_str(*last_end, &format!("{}{rendered}", style.separator(2)))
                }
                None => {
                    let replacement = match style {
                        Style::Compact => format!("[{rendered}]"),
                        Style::Pretty { indent, newline } => {
                            format!(
                                "[{newline}{}{rendered}{newline}{}]",
                                indent.repeat(2),
                                indent
                            )
                        }
                    };

                    edited.replace_range(open..=close, &replacement);
                }
            }

            return Self::new(edited);
        }

        let (open, members, close) = self.root()?;
        let array = style.render(&Json::Array(vec![entry.clone()]), 1);
        let member = match style {
            Style::Compact => format!("\"pluginHosts\":{array}"),
            Style::Pretty { .. } => format!("\"pluginHosts\": {array}"),
        };

        match members.last() {
            Some(last) => {
                edited.insert_str(last.value_end, &format!("{}{member}", style.separator(1)))
            }
            None => {
                let replacement = match style {
                    Style::Compact => format!("{{{member}}}"),
                    Style::Pretty { indent, newline } => {
                        format!("{{{newline}{indent}{member}{newline}}}")
                    }
                };

                edited.replace_range(open..=close, &replacement);
            }
        }

        Self::new(edited)
    }

    /// Remove one host entry, leaving every other entry untouched.
    pub fn remove_host(&self, host_id: &str) -> Result<Self, EditError> {
        let (_, elements, close) = self
            .hosts_array()?
            .ok_or_else(|| EditError::HostNotFound(host_id.to_owned()))?;
        let (start, end) = self
            .find_host(host_id)?
            .ok_or_else(|| EditError::HostNotFound(host_id.to_owned()))?;
        let index = elements
            .iter()
            .position(|span| *span == (start, end))
            .expect("the located host is one of the elements");

        Self::new(self.remove_element(&elements, close, index))
    }

    /// Delete element `index` with its separator, collapsing an emptied array to `[]`.
    fn remove_element(&self, elements: &[(usize, usize)], close: usize, index: usize) -> String {
        let mut edited = self.source.clone();

        if elements.len() == 1 {
            let open = self.source[..elements[0].0]
                .rfind('[')
                .expect("an element is preceded by its array bracket");

            edited.replace_range(open..=close, "[]");
        } else if index == 0 {
            edited.replace_range(elements[0].0..elements[1].0, "");
        } else {
            edited.replace_range(elements[index - 1].1..elements[index].1, "");
        }

        edited
    }

    /// Set a host's `enabled` flag. An absent flag means enabled, so enabling a host without
    /// the member changes nothing.
    pub fn set_host_enabled(&self, host_id: &str, enabled: bool) -> Result<Self, EditError> {
        let (start, end) = self
            .find_host(host_id)?
            .ok_or_else(|| EditError::HostNotFound(host_id.to_owned()))?;
        let scanner = self.scanner();
        let (members, _) = scanner.members(start)?;
        let literal = if enabled { "true" } else { "false" };
        let mut edited = self.source.clone();

        if let Some(member) = members.iter().find(|member| member.key == "enabled") {
            edited.replace_range(member.value_start..member.value_end, literal);
        } else if !enabled {
            let anchor = members
                .iter()
                .find(|member| member.key == "id")
                .map_or(end - 1, |member| member.value_end);
            let member = match self.style {
                Style::Compact => format!("\"enabled\":{literal}"),
                Style::Pretty { .. } => format!("\"enabled\": {literal}"),
            };

            edited.insert_str(anchor, &format!("{}{member}", self.style.separator(2)));
        }

        Self::new(edited)
    }

    /// Remove and append entries in one host's `operations` allowlist.
    ///
    /// Entries which are neither removed nor replaced keep their text, including
    /// `replaySafe`. New entries must be rendered by the caller.
    pub fn change_host_operations(
        &self,
        host_id: &str,
        remove: &[(String, String)],
        add: &[Json],
    ) -> Result<Self, EditError> {
        let mut current = Self::new(self.source.clone())?;

        for (plugin_id, operation) in remove {
            let Some((start, _)) = current.find_host(host_id)? else {
                return Err(EditError::HostNotFound(host_id.to_owned()));
            };
            let (members, _) = current.scanner().members(start)?;
            let Some(array) = members.iter().find(|member| member.key == "operations") else {
                continue;
            };
            let (elements, close) = current.scanner().elements(array.value_start)?;
            let found = elements.iter().position(|(from, to)| {
                serde_json::from_str::<Value>(&current.source[*from..*to]).is_ok_and(|value| {
                    value.get("pluginId").and_then(Value::as_str) == Some(plugin_id)
                        && value.get("operation").and_then(Value::as_str) == Some(operation)
                })
            });

            if let Some(index) = found {
                current = Self::new(current.remove_element(&elements, close, index))?;
            }
        }

        for entry in add {
            let Some((start, _)) = current.find_host(host_id)? else {
                return Err(EditError::HostNotFound(host_id.to_owned()));
            };
            let (members, _) = current.scanner().members(start)?;
            let array = members
                .iter()
                .find(|member| member.key == "operations")
                .ok_or_else(|| EditError::Syntax("host has no operations array".to_owned()))?;
            let (elements, close) = current.scanner().elements(array.value_start)?;
            let rendered = current.style.render(entry, 4);
            let mut edited = current.source.clone();

            match elements.last() {
                Some((_, last_end)) => {
                    edited.insert_str(
                        *last_end,
                        &format!("{}{rendered}", current.style.separator(4)),
                    );
                }
                None => {
                    let replacement = match &current.style {
                        Style::Compact => format!("[{rendered}]"),
                        Style::Pretty { indent, newline } => {
                            format!(
                                "[{newline}{}{rendered}{newline}{}]",
                                indent.repeat(4),
                                indent.repeat(3)
                            )
                        }
                    };

                    edited.replace_range(array.value_start..=close, &replacement);
                }
            }

            current = Self::new(edited)?;
        }

        Ok(current)
    }
}

/// A snapshot of a file used to detect concurrent modification.
pub struct Snapshot {
    path: PathBuf,
    contents: String,
}

impl Snapshot {
    pub fn contents(&self) -> &str {
        &self.contents
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> EditError + '_ {
    move |source| EditError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Read a configuration file, resolving symbolic links so a later write replaces the real file.
pub fn read(path: &Path) -> Result<Snapshot, EditError> {
    let resolved = fs::canonicalize(path).map_err(io_error(path))?;
    let contents = fs::read_to_string(&resolved).map_err(io_error(&resolved))?;

    Ok(Snapshot {
        path: resolved,
        contents,
    })
}

/// Replace the file with `contents` unless it changed since `snapshot` was read.
///
/// The new contents are written to a temporary file beside the original, flushed, given the
/// original's permissions (or owner-only permissions) and renamed over it. On any failure the
/// original file is left untouched and the temporary file is removed.
pub fn write_atomic(snapshot: &Snapshot, contents: &str) -> Result<(), EditError> {
    let path = &snapshot.path;
    let directory = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "remedia.config.json".to_owned());
    let temporary = directory.join(format!(".{name}.{}.tmp", std::process::id()));
    let permissions = fs::metadata(path).map_err(io_error(path))?.permissions();
    let result = (|| {
        let mut options = fs::OpenOptions::new();

        options.write(true).create_new(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.mode(0o600);
        }

        let mut file = options.open(&temporary).map_err(io_error(&temporary))?;

        file.write_all(contents.as_bytes())
            .map_err(io_error(&temporary))?;
        file.sync_all().map_err(io_error(&temporary))?;
        drop(file);
        fs::set_permissions(&temporary, permissions).map_err(io_error(&temporary))?;

        // Checked as late as possible: a change made after this point cannot be detected.
        if fs::read_to_string(path).map_err(io_error(path))? != snapshot.contents {
            return Err(EditError::Concurrent(path.clone()));
        }

        fs::rename(&temporary, path).map_err(io_error(path))
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }

    result
}

#[cfg(test)]
mod tests;

//! Resolves environment variable placeholders in a TOML document.

use std::{
  collections::{BTreeMap, BTreeSet},
  ffi::OsString,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum Error {
  #[error(transparent)]
  Document(#[from] toml::de::Error),
  #[error("environment variable `{0}` referenced by the configuration is not set")]
  NotFound(String),
  #[error("environment variable `{0}` is not valid UTF-8")]
  NotUnicode(String),
  #[error("environment variable `{0}` must contain a valid TOML value")]
  InvalidValue(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum State {
  #[default]
  Document,
  Comment,
  BasicString,
  LiteralString,
  MultiBasicString,
  MultiLiteralString,
}

struct Placeholder {
  marker: String,
  name: String,
}

pub(crate) fn resolve<I>(
  document: &str, variables: I,
) -> Result<(toml::Table, Vec<String>), Error>
where
  I: IntoIterator<Item = (OsString, OsString)>, {
  let variables = variables
    .into_iter()
    .filter_map(|(name, value)| name.into_string().ok().map(|name| (name, value)))
    .collect::<BTreeMap<_, _>>();
  let (masked, placeholders) = mask_bare_placeholders(document);
  let table = toml::from_str::<toml::Table>(&masked)?;

  let mut replacements = BTreeMap::new();
  for placeholder in placeholders {
    let value = variable(&variables, &placeholder.name)?;
    let value = parse_toml_value(&placeholder.name, value)?;
    replacements.insert(placeholder.marker, (placeholder.name, value));
  }

  let mut value = toml::Value::Table(table);
  let mut referenced = BTreeSet::new();
  resolve_values(&mut value, &variables, &replacements, &mut referenced)?;
  match value {
    toml::Value::Table(table) => Ok((table, referenced.into_iter().collect())),
    _ => unreachable!("the root TOML value is always a table"),
  }
}

fn variable<'a>(variables: &'a BTreeMap<String, OsString>, name: &str) -> Result<&'a str, Error> {
  variables
    .get(name)
    .ok_or_else(|| Error::NotFound(name.to_owned()))?
    .to_str()
    .ok_or_else(|| Error::NotUnicode(name.to_owned()))
}

fn parse_toml_value(name: &str, value: &str) -> Result<toml::Value, Error> {
  let document = format!("value = {value}");
  let mut table =
    toml::from_str::<toml::Table>(&document).map_err(|_| Error::InvalidValue(name.to_owned()))?;
  let resolved = table
    .remove("value")
    .ok_or_else(|| Error::InvalidValue(name.to_owned()))?;
  if table.is_empty() {
    Ok(resolved)
  } else {
    Err(Error::InvalidValue(name.to_owned()))
  }
}

fn resolve_values(
  value: &mut toml::Value, variables: &BTreeMap<String, OsString>,
  replacements: &BTreeMap<String, (String, toml::Value)>, referenced: &mut BTreeSet<String>,
) -> Result<(), Error> {
  match value {
    toml::Value::String(text) => {
      if let Some((name, replacement)) = replacements.get(text) {
        referenced.insert(name.clone());
        *value = replacement.clone();
        return Ok(());
      }
      if let Some(name) = placeholder_name(text) {
        let name = name.to_owned();
        *value = toml::Value::String(variable(variables, &name)?.to_owned());
        referenced.insert(name);
      }
    }
    toml::Value::Array(values) => {
      for value in values {
        resolve_values(value, variables, replacements, referenced)?;
      }
    }
    toml::Value::Table(table) => {
      for (_, value) in table.iter_mut() {
        resolve_values(value, variables, replacements, referenced)?;
      }
    }
    _ => {}
  }
  Ok(())
}

fn placeholder_name(value: &str) -> Option<&str> {
  let name = value.strip_prefix("${")?.strip_suffix('}')?;
  valid_name(name).then_some(name)
}

fn valid_name(name: &str) -> bool {
  let mut characters = name.chars();
  characters
    .next()
    .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
    && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

/// Replaces bare placeholders with unique TOML strings so the document can be
/// parsed safely. Quoted placeholders remain untouched and are resolved after
/// parsing, which preserves environment values as strings without escaping
/// them into the source document.
fn mask_bare_placeholders(document: &str) -> (String, Vec<Placeholder>) {
  let marker_prefix = marker_prefix(document);
  let mut masked = String::with_capacity(document.len());
  let mut placeholders = Vec::new();
  let mut state = State::Document;
  let mut index = 0;

  while index < document.len() {
    let remainder = &document[index..];
    match state {
      State::Document => {
        if remainder.starts_with("\"\"\"") {
          masked.push_str("\"\"\"");
          index += 3;
          state = State::MultiBasicString;
        } else if remainder.starts_with("'''") {
          masked.push_str("'''");
          index += 3;
          state = State::MultiLiteralString;
        } else if remainder.starts_with('"') {
          masked.push('"');
          index += 1;
          state = State::BasicString;
        } else if remainder.starts_with('\'') {
          masked.push('\'');
          index += 1;
          state = State::LiteralString;
        } else if remainder.starts_with('#') {
          masked.push('#');
          index += 1;
          state = State::Comment;
        } else if let Some((name, consumed)) = bare_placeholder(remainder) {
          let marker = format!("{marker_prefix}{}", placeholders.len());
          masked.push('"');
          masked.push_str(&marker);
          masked.push('"');
          placeholders.push(Placeholder {
            marker,
            name: name.to_owned(),
          });
          index += consumed;
        } else {
          push_character(&mut masked, document, &mut index);
        }
      }
      State::Comment => {
        push_character(&mut masked, document, &mut index);
        if masked.ends_with('\n') {
          state = State::Document;
        }
      }
      State::BasicString => {
        if remainder.starts_with('\\') {
          push_character(&mut masked, document, &mut index);
          if index < document.len() {
            push_character(&mut masked, document, &mut index);
          }
        } else {
          push_character(&mut masked, document, &mut index);
          if remainder.starts_with('"') {
            state = State::Document;
          }
        }
      }
      State::LiteralString => {
        push_character(&mut masked, document, &mut index);
        if remainder.starts_with('\'') {
          state = State::Document;
        }
      }
      State::MultiBasicString => {
        if remainder.starts_with("\"\"\"") {
          masked.push_str("\"\"\"");
          index += 3;
          state = State::Document;
        } else if remainder.starts_with('\\') {
          push_character(&mut masked, document, &mut index);
          if index < document.len() {
            push_character(&mut masked, document, &mut index);
          }
        } else {
          push_character(&mut masked, document, &mut index);
        }
      }
      State::MultiLiteralString => {
        if remainder.starts_with("'''") {
          masked.push_str("'''");
          index += 3;
          state = State::Document;
        } else {
          push_character(&mut masked, document, &mut index);
        }
      }
    }
  }

  (masked, placeholders)
}

fn marker_prefix(document: &str) -> String {
  let mut suffix = 0;
  loop {
    let prefix = format!("__RET2SHELL_ENV_{suffix}_");
    if !document.contains(&prefix) {
      return prefix;
    }
    suffix += 1;
  }
}

fn bare_placeholder(value: &str) -> Option<(&str, usize)> {
  let remainder = value.strip_prefix("${")?;
  let end = remainder.find('}')?;
  let name = &remainder[..end];
  valid_name(name).then_some((name, end + 3))
}

fn push_character(target: &mut String, source: &str, index: &mut usize) {
  let character = source[*index..]
    .chars()
    .next()
    .expect("the source index is in bounds");
  target.push(character);
  *index += character.len_utf8();
}

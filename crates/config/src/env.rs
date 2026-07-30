//! Configuration overrides from environment variables.
//!
//! Every configuration field can be overridden by an environment variable
//! named `R2S_CONFIG__<SECTION>__<FIELD>`, nested fields use additional `__`
//! separators, e.g. `R2S_CONFIG__CLUSTER__REGISTRY__PASSWORD`.
//!
//! The variables are not parsed on their own: they are layered on top of the
//! parsed configuration file and interpreted while the configuration structs
//! are deserialized, so that every value is parsed as the type of the field it
//! overrides. Strings are taken verbatim, booleans and numbers are parsed from
//! their textual representation, enums accept their variant names, and
//! composite fields accept TOML value syntax such as `['a', 'b']`.

use std::{
  cell::RefCell,
  collections::{BTreeMap, BTreeSet, btree_map},
  ffi::OsString,
  fmt::Display,
  rc::Rc,
};

use serde::{
  Deserialize,
  de::{
    DeserializeOwned, DeserializeSeed, Deserializer, IntoDeserializer, MapAccess, Visitor,
    value::StringDeserializer,
  },
  forward_to_deserialize_any,
};
use thiserror::Error;

/// Prefix of the environment variables that override configuration fields.
const PREFIX: &str = "R2S_CONFIG__";

/// Separator between the path segments of an overridden field.
const SEPARATOR: &str = "__";

/// Errors reported while overriding the configuration with the environment.
///
/// The configuration holds credentials, so the messages refer to the variable
/// that failed but never contain its value.
#[derive(Debug, Error)]
pub(crate) enum EnvError {
  #[error("configuration environment variable `{0}` does not match any field")]
  Unknown(String),
  #[error("configuration environment variable `{name}` cannot be parsed as {expected}")]
  Invalid { name: String, expected: String },
  #[error(transparent)]
  File(#[from] toml::de::Error),
  #[error("{0}")]
  Other(String),
}

impl serde::de::Error for EnvError {
  fn custom<T>(message: T) -> Self
  where
    T: Display, {
    EnvError::Other(message.to_string())
  }
}

/// The overrides collected from the environment, keyed by the path of the
/// field they replace.
pub(crate) struct Overrides(BTreeMap<Vec<String>, (String, String)>);

impl Overrides {
  /// Collects the overrides from the given environment variables, ignoring the
  /// ones that are not addressed to the configuration.
  pub(crate) fn collect<I>(variables: I) -> Result<Self, EnvError>
  where
    I: IntoIterator<Item = (OsString, OsString)>, {
    let mut overrides = BTreeMap::new();
    for (name, value) in variables {
      let Ok(name) = name.into_string() else {
        continue;
      };
      if !name.starts_with(PREFIX) {
        continue;
      }
      let Some(path) = field_path(&name) else {
        return Err(EnvError::Unknown(name));
      };
      let Ok(value) = value.into_string() else {
        return Err(EnvError::Invalid {
          name,
          expected: "valid UTF-8 text".to_owned(),
        });
      };
      overrides.insert(path, (name, value));
    }
    Ok(Self(overrides))
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.0.is_empty()
  }

  /// Deserializes the configuration from `document` with the overrides layered
  /// on top of it.
  pub(crate) fn apply<T>(self, document: toml::Table) -> Result<T, EnvError>
  where
    T: DeserializeOwned, {
    let mut root = document
      .into_iter()
      .map(|(key, value)| (key, Layer::File(value)))
      .collect::<BTreeMap<_, _>>();
    let pending = Rc::new(Pending::default());
    for (path, (name, value)) in self.0 {
      pending.insert(&name);
      insert(&mut root, &path, Layer::Environment { name, value });
    }

    let deserializer = LayerDeserializer::new(Layer::Table(root), Rc::clone(&pending));
    let config = T::deserialize(deserializer);
    match pending.remaining() {
      Some(name) => Err(EnvError::Unknown(name)),
      None => config,
    }
  }
}

/// Splits the name of an environment variable into the path of the field it
/// overrides.
fn field_path(name: &str) -> Option<Vec<String>> {
  let path = name
    .strip_prefix(PREFIX)?
    .split(SEPARATOR)
    .map(str::to_ascii_lowercase)
    .collect::<Vec<_>>();
  path
    .iter()
    .all(|segment| !segment.is_empty())
    .then_some(path)
}

/// Inserts an override into the configuration tree, turning the layers along
/// the path into tables.
fn insert(table: &mut BTreeMap<String, Layer>, path: &[String], leaf: Layer) {
  match path {
    [] => {}
    [field] => {
      table.insert(field.clone(), leaf);
    }
    [segment, rest @ ..] => {
      let mut nested = table
        .remove(segment)
        .map_or_else(BTreeMap::new, Layer::into_table);
      insert(&mut nested, rest, leaf);
      table.insert(segment.clone(), Layer::Table(nested));
    }
  }
}

/// A node of the configuration tree that is being deserialized.
enum Layer {
  /// A table whose entries come from the file, from the environment, or both.
  Table(BTreeMap<String, Layer>),
  /// A value that comes from the configuration file, typed by TOML itself.
  File(toml::Value),
  /// A value that comes from the environment, typed by the field it overrides.
  Environment { name: String, value: String },
}

impl Layer {
  /// Turns the layer into a table, so that overrides can be inserted into it.
  fn into_table(self) -> BTreeMap<String, Layer> {
    match self {
      Layer::Table(table) => table,
      Layer::File(toml::Value::Table(table)) => table
        .into_iter()
        .map(|(key, value)| (key, Layer::File(value)))
        .collect(),
      _ => BTreeMap::new(),
    }
  }
}

/// The overrides that have not been applied to any configuration field yet.
#[derive(Default)]
struct Pending(RefCell<BTreeSet<String>>);

impl Pending {
  fn insert(&self, name: &str) {
    self.0.borrow_mut().insert(name.to_owned());
  }

  fn applied(&self, name: &str) {
    self.0.borrow_mut().remove(name);
  }

  fn remaining(&self) -> Option<String> {
    self.0.borrow().first().cloned()
  }
}

/// Dispatches a [`Deserializer`] method to the deserializer of the layer.
macro_rules! forward_to_layer {
  ($($method:ident($($argument:ident: $type:ty),*);)*) => {
    $(
      fn $method<V>(self, $($argument: $type,)* visitor: V) -> Result<V::Value, Self::Error>
      where
        V: Visitor<'de>, {
        let Self { layer, pending } = self;
        match layer {
          Layer::Table(table) => {
            TableDeserializer::new(table, pending).$method($($argument,)* visitor)
          }
          Layer::File(value) => Ok(value.$method($($argument,)* visitor)?),
          Layer::Environment { name, value } => {
            EnvironmentDeserializer::new(name, value, pending).$method($($argument,)* visitor)
          }
        }
      }
    )*
  };
}

/// Deserializes any layer of the configuration tree.
struct LayerDeserializer {
  layer: Layer,
  pending: Rc<Pending>,
}

impl LayerDeserializer {
  fn new(layer: Layer, pending: Rc<Pending>) -> Self {
    Self { layer, pending }
  }
}

impl<'de> Deserializer<'de> for LayerDeserializer {
  type Error = EnvError;

  forward_to_layer! {
    deserialize_any();
    deserialize_bool();
    deserialize_i8();
    deserialize_i16();
    deserialize_i32();
    deserialize_i64();
    deserialize_i128();
    deserialize_u8();
    deserialize_u16();
    deserialize_u32();
    deserialize_u64();
    deserialize_u128();
    deserialize_f32();
    deserialize_f64();
    deserialize_char();
    deserialize_str();
    deserialize_string();
    deserialize_bytes();
    deserialize_byte_buf();
    deserialize_option();
    deserialize_unit();
    deserialize_seq();
    deserialize_map();
    deserialize_identifier();
    deserialize_ignored_any();
    deserialize_unit_struct(name: &'static str);
    deserialize_newtype_struct(name: &'static str);
    deserialize_tuple(length: usize);
    deserialize_tuple_struct(name: &'static str, length: usize);
    deserialize_struct(name: &'static str, fields: &'static [&'static str]);
    deserialize_enum(name: &'static str, variants: &'static [&'static str]);
  }
}

/// Deserializes a table that mixes file and environment entries.
struct TableDeserializer {
  table: BTreeMap<String, Layer>,
  pending: Rc<Pending>,
}

impl TableDeserializer {
  fn new(table: BTreeMap<String, Layer>, pending: Rc<Pending>) -> Self {
    Self { table, pending }
  }
}

impl<'de> Deserializer<'de> for TableDeserializer {
  type Error = EnvError;

  fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    visitor.visit_map(TableAccess {
      entries: self.table.into_iter(),
      value: None,
      pending: self.pending,
    })
  }

  fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    visitor.visit_some(self)
  }

  fn deserialize_newtype_struct<V>(
    self, _name: &'static str, visitor: V,
  ) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    visitor.visit_newtype_struct(self)
  }

  forward_to_deserialize_any! {
    bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf unit
    unit_struct seq tuple tuple_struct map struct enum identifier ignored_any
  }
}

/// Walks the entries of a table layer.
struct TableAccess {
  entries: btree_map::IntoIter<String, Layer>,
  value: Option<Layer>,
  pending: Rc<Pending>,
}

impl<'de> MapAccess<'de> for TableAccess {
  type Error = EnvError;

  fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
  where
    K: DeserializeSeed<'de>, {
    let Some((key, value)) = self.entries.next() else {
      return Ok(None);
    };
    self.value = Some(value);
    let key: StringDeserializer<Self::Error> = key.into_deserializer();
    seed.deserialize(key).map(Some)
  }

  fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
  where
    V: DeserializeSeed<'de>, {
    let value = self.value.take().ok_or_else(|| {
      EnvError::Other("configuration value is requested before its key".to_owned())
    })?;
    seed.deserialize(LayerDeserializer::new(value, Rc::clone(&self.pending)))
  }

  fn size_hint(&self) -> Option<usize> {
    Some(self.entries.len())
  }
}

/// Parses a scalar override with the type requested by the configuration
/// field.
macro_rules! deserialize_parsed {
  ($($method:ident => $visit:ident($type:ty), $expected:literal;)*) => {
    $(
      fn $method<V>(self, visitor: V) -> Result<V::Value, Self::Error>
      where
        V: Visitor<'de>, {
        self.applied();
        let value = self
          .value
          .trim()
          .parse::<$type>()
          .map_err(|_| self.invalid($expected))?;
        visitor.$visit(value)
      }
    )*
  };
}

/// Deserializes an environment override as the type of the field it replaces.
struct EnvironmentDeserializer {
  name: String,
  value: String,
  pending: Rc<Pending>,
}

impl EnvironmentDeserializer {
  fn new(name: String, value: String, pending: Rc<Pending>) -> Self {
    Self {
      name,
      value,
      pending,
    }
  }

  /// Records that the override reached a configuration field.
  fn applied(&self) {
    self.pending.applied(&self.name);
  }

  fn invalid(&self, expected: impl Into<String>) -> EnvError {
    EnvError::Invalid {
      name: self.name.clone(),
      expected: expected.into(),
    }
  }

  /// Parses the override as a TOML value, for the fields that a plain string
  /// cannot represent.
  fn parse(&self) -> Option<toml::Value> {
    let deserializer = toml::de::ValueDeserializer::parse(&self.value).ok()?;
    toml::Value::deserialize(deserializer).ok()
  }
}

impl<'de> Deserializer<'de> for EnvironmentDeserializer {
  type Error = EnvError;

  deserialize_parsed! {
    deserialize_i8 => visit_i8(i8), "an 8-bit signed integer";
    deserialize_i16 => visit_i16(i16), "a 16-bit signed integer";
    deserialize_i32 => visit_i32(i32), "a 32-bit signed integer";
    deserialize_i64 => visit_i64(i64), "a 64-bit signed integer";
    deserialize_i128 => visit_i128(i128), "a 128-bit signed integer";
    deserialize_u8 => visit_u8(u8), "an 8-bit unsigned integer";
    deserialize_u16 => visit_u16(u16), "a 16-bit unsigned integer";
    deserialize_u32 => visit_u32(u32), "a 32-bit unsigned integer";
    deserialize_u64 => visit_u64(u64), "a 64-bit unsigned integer";
    deserialize_u128 => visit_u128(u128), "a 128-bit unsigned integer";
    deserialize_f32 => visit_f32(f32), "a 32-bit floating point number";
    deserialize_f64 => visit_f64(f64), "a 64-bit floating point number";
    deserialize_char => visit_char(char), "a single character";
  }

  fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.applied();
    // The field gives no hint about its type, so fall back to the TOML syntax
    // and keep the value as a string when it is not a TOML value.
    match self.parse() {
      Some(value) => Ok(value.deserialize_any(visitor)?),
      None => visitor.visit_str(&self.value),
    }
  }

  fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.applied();
    let value = self.value.trim();
    if value.eq_ignore_ascii_case("true") {
      visitor.visit_bool(true)
    } else if value.eq_ignore_ascii_case("false") {
      visitor.visit_bool(false)
    } else {
      Err(self.invalid("a boolean"))
    }
  }

  fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.applied();
    visitor.visit_str(&self.value)
  }

  fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.deserialize_str(visitor)
  }

  fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.applied();
    visitor.visit_bytes(self.value.as_bytes())
  }

  fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.deserialize_bytes(visitor)
  }

  fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    visitor.visit_some(self)
  }

  fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.applied();
    visitor.visit_unit()
  }

  fn deserialize_unit_struct<V>(
    self, _name: &'static str, visitor: V,
  ) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.deserialize_unit(visitor)
  }

  fn deserialize_newtype_struct<V>(
    self, _name: &'static str, visitor: V,
  ) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    visitor.visit_newtype_struct(self)
  }

  fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.applied();
    let value = self.parse().ok_or_else(|| self.invalid("a TOML array"))?;
    value
      .deserialize_seq(visitor)
      .map_err(|_| self.invalid("a TOML array"))
  }

  fn deserialize_tuple<V>(self, _length: usize, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.deserialize_seq(visitor)
  }

  fn deserialize_tuple_struct<V>(
    self, _name: &'static str, _length: usize, visitor: V,
  ) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.deserialize_seq(visitor)
  }

  fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.applied();
    let value = self.parse().ok_or_else(|| self.invalid("a TOML table"))?;
    value
      .deserialize_map(visitor)
      .map_err(|_| self.invalid("a TOML table"))
  }

  fn deserialize_struct<V>(
    self, _name: &'static str, _fields: &'static [&'static str], visitor: V,
  ) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.deserialize_map(visitor)
  }

  fn deserialize_enum<V>(
    self, _name: &'static str, variants: &'static [&'static str], visitor: V,
  ) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.applied();
    let value = self.value.trim();
    let variant = variants
      .iter()
      .find(|variant| variant.eq_ignore_ascii_case(value))
      .ok_or_else(|| self.invalid(format!("one of {}", variants.join(", "))))?;
    visitor.visit_enum((*variant).into_deserializer())
  }

  fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    self.deserialize_str(visitor)
  }

  fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
  where
    V: Visitor<'de>, {
    // The override did not reach a configuration field, so it stays pending
    // and is reported as unknown once the configuration is deserialized.
    visitor.visit_unit()
  }
}

#[cfg(test)]
mod tests {
  use std::{collections::BTreeMap, ffi::OsString};

  use serde::Deserialize;

  use super::{EnvError, Overrides};

  #[derive(Debug, Deserialize)]
  #[serde(rename_all = "snake_case")]
  enum Mode {
    Direct,
    Proxied,
  }

  #[derive(Debug, Deserialize)]
  struct Section {
    hosts: Vec<String>,
    labels: BTreeMap<String, String>,
    ratio: f64,
    mode: Mode,
  }

  #[derive(Debug, Deserialize)]
  struct Document {
    section: Section,
  }

  fn document() -> toml::Table {
    toml::from_str(
      r#"
[section]
hosts = ['a.example']
labels = { env = 'dev' }
ratio = 0.5
mode = 'direct'
"#,
    )
    .unwrap()
  }

  fn overrides(entries: &[(&str, &str)]) -> Overrides {
    Overrides::collect(
      entries
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value))),
    )
    .unwrap()
  }

  #[test]
  fn composite_fields_are_parsed_with_toml_syntax() {
    let document: Document = overrides(&[
      ("R2S_CONFIG__SECTION__HOSTS", "['b.example', 'c.example']"),
      ("R2S_CONFIG__SECTION__LABELS", "{ env = 'prod' }"),
      ("R2S_CONFIG__SECTION__RATIO", "1.5"),
      ("R2S_CONFIG__SECTION__MODE", "PROXIED"),
    ])
    .apply(document())
    .unwrap();

    assert_eq!(document.section.hosts, ["b.example", "c.example"]);
    assert_eq!(
      document.section.labels.get("env").map(String::as_str),
      Some("prod")
    );
    assert_eq!(document.section.ratio, 1.5);
    assert!(matches!(document.section.mode, Mode::Proxied));
  }

  #[test]
  fn composite_fields_reject_values_without_leaking_them() {
    for (name, value) in [
      ("R2S_CONFIG__SECTION__HOSTS", "b.example"),
      ("R2S_CONFIG__SECTION__LABELS", "env=prod"),
      ("R2S_CONFIG__SECTION__RATIO", "half"),
      ("R2S_CONFIG__SECTION__MODE", "sidecar"),
    ] {
      let error = overrides(&[(name, value)])
        .apply::<Document>(document())
        .unwrap_err();

      assert!(matches!(error, EnvError::Invalid { .. }), "{error}");
      let error = error.to_string();
      assert!(error.contains(name), "{error}");
      assert!(!error.contains(value), "{error}");
    }
  }

  #[test]
  fn enum_fields_report_the_variants_they_accept() {
    let error = overrides(&[("R2S_CONFIG__SECTION__MODE", "sidecar")])
      .apply::<Document>(document())
      .unwrap_err()
      .to_string();

    assert!(error.contains("direct, proxied"), "{error}");
  }
}

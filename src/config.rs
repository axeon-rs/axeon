//! Typed configuration from environment variables, with `.env` support.
//!
//! ```rust,no_run
//! use axeon::config;
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct Config {
//!     database_url: String,          // DATABASE_URL (required)
//!     port: u16,                     // PORT
//!     #[serde(default)]
//!     debug: bool,                   // DEBUG, optional: false if unset
//!     allowed_origins: Vec<String>,  // ALLOWED_ORIGINS=a.com,b.com
//!     redis_url: Option<String>,     // REDIS_URL, optional
//! }
//!
//! fn main() {
//!     config::load_dotenv().expect("invalid .env file"); // optional
//!     let config: Config = config::from_env().expect("invalid configuration");
//! }
//! ```
//!
//! Field names map to upper-case variable names (`database_url` ←
//! `DATABASE_URL`). Numbers and booleans (`true`/`false`/`1`/`0`) are
//! parsed, lists are comma-separated, and `Option` fields are `None` when
//! unset. All missing or invalid variables are reported at once.

use serde::de::{self, DeserializeOwned, Deserializer, IntoDeserializer, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// What was wrong with the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

impl de::Error for ConfigError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        ConfigError { message: msg.to_string() }
    }

    fn missing_field(field: &'static str) -> Self {
        ConfigError { message: format!("missing environment variable {}", field.to_ascii_uppercase()) }
    }
}

/// Reads configuration from environment variables.
pub fn from_env<T: DeserializeOwned>() -> Result<T, ConfigError> {
    from_iter(std::env::vars(), "")
}

/// Like [`from_env`], reading `PREFIX_NAME` variables for field `name`,
/// e.g. `from_env_prefixed::<Config>("APP_")` reads `APP_PORT` for `port`.
pub fn from_env_prefixed<T: DeserializeOwned>(prefix: &str) -> Result<T, ConfigError> {
    from_iter(std::env::vars(), prefix)
}

/// Reads configuration from `(name, value)` pairs, e.g. in tests.
pub fn from_iter<T, I>(vars: I, prefix: &str) -> Result<T, ConfigError>
where
    T: DeserializeOwned,
    I: IntoIterator<Item = (String, String)>,
{
    let prefix = prefix.to_ascii_uppercase();
    let vars: BTreeMap<String, String> = vars
        .into_iter()
        .filter_map(|(name, value)| {
            let name = name.to_ascii_uppercase();
            name.strip_prefix(&prefix).map(|rest| (rest.to_ascii_lowercase(), value))
        })
        .collect();
    let mut errors = Vec::new();
    let result = T::deserialize(EnvMap { vars: &vars, prefix: &prefix, errors: &mut errors });
    match result {
        Ok(value) if errors.is_empty() => Ok(value),
        Ok(_) => Err(ConfigError { message: errors.join("; ") }),
        Err(error) => {
            errors.push(error.message.replace("environment variable ", &format!("environment variable {}", prefix)));
            errors.dedup();
            Err(ConfigError { message: errors.join("; ") })
        }
    }
}

/// Loads `KEY=VALUE` lines from `.env` in the current directory into the
/// environment, without overriding variables that are already set. Returns
/// `Ok(false)` if there is no `.env` file. Call it at the start of `main`,
/// before starting threads.
pub fn load_dotenv() -> Result<bool, ConfigError> {
    load_dotenv_from(".env")
}

/// Like [`load_dotenv`] for a specific file.
pub fn load_dotenv_from(path: impl AsRef<Path>) -> Result<bool, ConfigError> {
    let contents = match std::fs::read_to_string(path.as_ref()) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(ConfigError { message: format!("can't read {}: {}", path.as_ref().display(), e) }),
    };
    for (number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            return Err(ConfigError { message: format!("{} line {}: expected KEY=VALUE", path.as_ref().display(), number + 1) });
        };
        let key = key.trim();
        let mut value = value.trim();
        if value.len() >= 2 && (value.starts_with('"') && value.ends_with('"') || value.starts_with('\'') && value.ends_with('\'')) {
            value = &value[1..value.len() - 1];
        } else if let Some((before, _comment)) = value.split_once(" #") {
            value = before.trim_end();
        }
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
    Ok(true)
}

struct EnvMap<'a> {
    vars: &'a BTreeMap<String, String>,
    prefix: &'a str,
    errors: &'a mut Vec<String>,
}

impl<'de> Deserializer<'de> for EnvMap<'_> {
    type Error = ConfigError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConfigError> {
        self.deserialize_map(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ConfigError> {
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConfigError> {
        let entries: Vec<(&String, &String)> = self.vars.iter().collect();
        visitor.visit_map(EnvEntries { entries, index: 0, prefix: self.prefix, errors: self.errors })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct enum identifier ignored_any
    }
}

struct EnvEntries<'a> {
    entries: Vec<(&'a String, &'a String)>,
    index: usize,
    prefix: &'a str,
    errors: &'a mut Vec<String>,
}

impl<'de> MapAccess<'de> for EnvEntries<'_> {
    type Error = ConfigError;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, ConfigError> {
        match self.entries.get(self.index) {
            Some((name, _)) => seed.deserialize(name.as_str().into_deserializer()).map(Some),
            None => Ok(None),
        }
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, ConfigError> {
        let (name, value) = self.entries[self.index];
        self.index += 1;
        let variable = format!("{}{}", self.prefix, name.to_ascii_uppercase());
        seed.deserialize(EnvValue { value }).map_err(|e| {
            let error = format!("invalid {}: {}", variable, e);
            self.errors.push(error.clone());
            ConfigError { message: error }
        })
    }
}

/// One variable's value, parsed into whatever type the field wants.
struct EnvValue<'a> {
    value: &'a str,
}

impl EnvValue<'_> {
    fn parse<T: std::str::FromStr>(&self, what: &str) -> Result<T, ConfigError> {
        self.value.trim().parse().map_err(|_| de::Error::custom(format!("expected {}, got {:?}", what, self.value)))
    }
}

macro_rules! parse_number {
    ($($method:ident => $visit:ident : $ty:ty),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConfigError> {
                visitor.$visit(self.parse::<$ty>(stringify!($ty))?)
            }
        )*
    };
}

impl<'de> Deserializer<'de> for EnvValue<'_> {
    type Error = ConfigError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConfigError> {
        visitor.visit_string(self.value.to_string())
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConfigError> {
        match self.value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => visitor.visit_bool(true),
            "false" | "0" | "no" | "off" | "" => visitor.visit_bool(false),
            _ => Err(de::Error::custom(format!("expected true or false, got {:?}", self.value))),
        }
    }

    parse_number! {
        deserialize_i8 => visit_i8: i8, deserialize_i16 => visit_i16: i16,
        deserialize_i32 => visit_i32: i32, deserialize_i64 => visit_i64: i64,
        deserialize_u8 => visit_u8: u8, deserialize_u16 => visit_u16: u16,
        deserialize_u32 => visit_u32: u32, deserialize_u64 => visit_u64: u64,
        deserialize_f32 => visit_f32: f32, deserialize_f64 => visit_f64: f64,
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConfigError> {
        if self.value.is_empty() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConfigError> {
        let items: Vec<&str> = self.value.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        visitor.visit_seq(EnvSeq { items: items.into_iter() })
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, ConfigError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ConfigError> {
        visitor.visit_enum(self.value.trim().into_deserializer())
    }

    serde::forward_to_deserialize_any! {
        i128 u128 char str string bytes byte_buf unit unit_struct tuple
        tuple_struct map struct identifier ignored_any
    }
}

struct EnvSeq<'a> {
    items: std::vec::IntoIter<&'a str>,
}

impl<'de> SeqAccess<'de> for EnvSeq<'_> {
    type Error = ConfigError;

    fn next_element_seed<T: de::DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, ConfigError> {
        match self.items.next() {
            Some(value) => seed.deserialize(EnvValue { value }).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "lowercase")]
    enum Mode {
        Dev,
        Prod,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Config {
        database_url: String,
        port: u16,
        #[serde(default)]
        debug: bool,
        origins: Vec<String>,
        redis_url: Option<String>,
        ratio: f64,
        mode: Mode,
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn parses_typed_values() {
        let config: Config = from_iter(
            vars(&[
                ("APP_DATABASE_URL", "postgres://x"),
                ("APP_PORT", "8080"),
                ("APP_ORIGINS", "a.com, b.com"),
                ("APP_RATIO", "0.5"),
                ("APP_MODE", "prod"),
                ("UNRELATED", "x"),
            ]),
            "APP_",
        )
        .unwrap();
        assert_eq!(
            config,
            Config {
                database_url: "postgres://x".into(),
                port: 8080,
                debug: false,
                origins: vec!["a.com".into(), "b.com".into()],
                redis_url: None,
                ratio: 0.5,
                mode: Mode::Prod,
            }
        );
    }

    #[test]
    fn reports_missing_and_invalid_variables() {
        let error = from_iter::<Config, _>(
            vars(&[("APP_PORT", "eighty"), ("APP_ORIGINS", ""), ("APP_RATIO", "1"), ("APP_MODE", "dev")]),
            "APP_",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("invalid APP_PORT: expected u16, got \"eighty\""), "{}", error);

        let error = from_iter::<Config, _>(vars(&[("APP_PORT", "1"), ("APP_RATIO", "1"), ("APP_MODE", "dev"), ("APP_ORIGINS", "")]), "APP_")
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing environment variable APP_DATABASE_URL"), "{}", error);
    }

    #[test]
    fn loads_dotenv_files_without_overriding() {
        let dir = std::env::temp_dir().join(format!("axeon-dotenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(".env");
        std::fs::write(&file, "# comment\nAXEON_TEST_A=one\nexport AXEON_TEST_B=\"two words\"\nAXEON_TEST_C=three # note\n").unwrap();
        std::env::set_var("AXEON_TEST_A", "preset");
        assert!(load_dotenv_from(&file).unwrap());
        assert_eq!(std::env::var("AXEON_TEST_A").unwrap(), "preset");
        assert_eq!(std::env::var("AXEON_TEST_B").unwrap(), "two words");
        assert_eq!(std::env::var("AXEON_TEST_C").unwrap(), "three");
        assert!(!load_dotenv_from(dir.join("missing.env")).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    mod properties {
        use super::super::*;
        use proptest::prelude::*;
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Any {
            a: Option<u16>,
            b: Option<bool>,
            c: Option<Vec<i32>>,
            d: Option<f64>,
            e: Option<String>,
        }

        proptest! {
            /// Arbitrary values give a value or an error, never a panic.
            #[test]
            fn arbitrary_values_never_panic(a in ".{0,12}", b in ".{0,8}", c in ".{0,16}", d in ".{0,8}") {
                let vars = vec![("A".to_string(), a), ("B".to_string(), b), ("C".to_string(), c), ("D".to_string(), d)];
                let _ = from_iter::<Any, _>(vars, "");
            }
        }
    }
}

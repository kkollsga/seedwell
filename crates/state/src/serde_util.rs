//! serde adapters for types that live in other crates or want friendlier TOML.

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use trs_classify::Category;

/// `Option<Category>` as its lowercase name.
pub mod category_opt {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<Category>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(c) => s.serialize_str(c.as_str()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Category>, D::Error> {
        let s = String::deserialize(d)?;
        Category::parse(&s).map(Some).ok_or_else(|| unknown_category(&s))
    }
}

fn unknown_category<E: de::Error>(s: &str) -> E {
    E::custom(format!(
        "unknown category `{s}` (expected one of: movie, tv, music, audiobook, book, software, other)"
    ))
}

/// `Vec<Category>`; a single string is accepted as a one-element list.
pub mod category_vec {
    use super::*;

    pub fn serialize<S: Serializer>(v: &[Category], s: S) -> Result<S::Ok, S::Error> {
        let names: Vec<&str> = v.iter().map(|c| c.as_str()).collect();
        names.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Category>, D::Error> {
        let names = super::string_or_vec::deserialize(d)?;
        names.iter().map(|s| Category::parse(s).ok_or_else(|| unknown_category(s))).collect()
    }
}

/// `Vec<String>`; a single string is accepted as a one-element list.
pub mod string_or_vec {
    use super::*;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Vec<String>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or a list of strings")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<String>, E> {
                Ok(vec![v.to_string()])
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
                let mut out = Vec::new();
                // Cap what a hostile file can make us allocate before validation runs.
                while let Some(s) = seq.next_element::<String>()? {
                    if out.len() >= 1024 {
                        return Err(de::Error::custom("list has more than 1024 entries"));
                    }
                    out.push(s);
                }
                Ok(out)
            }
        }
        d.deserialize_any(V)
    }
}


//! Torrent selection for every command that acts on torrents.
//!
//! A command names torrents by id prefix (6+ hex characters), by name (a substring of the title or
//! the name, any case) or with filters: `--all`, `--tracker X`, `--state S`, `--category C`. A term
//! that matches more than one torrent is an error that lists the candidates (exit 3), never a guess.

use serde_json::json;
use trs_control::proto::ListFilter;
use trs_control::types::TransferDto;

use crate::args::{Opts, Usage};

/// How many candidates an ambiguity error lists.
const MAX_CANDIDATES: usize = 8;

/// What the user typed to pick torrents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Ids, id prefixes or names, one torrent each.
    pub terms: Vec<String>,
    /// `--all`: every torrent.
    pub all: bool,
    /// `--tracker X`: host or favorite name contains X.
    pub tracker: Option<String>,
    /// `--state S`: one of the list filters.
    pub state: Option<String>,
    /// `--category C`.
    pub category: Option<String>,
}

/// Why a selection did not resolve.
#[derive(Debug, PartialEq)]
pub enum SelectError {
    /// Nothing matches.
    NotFound(String),
    /// More than one torrent matches a term (or a single-target command got several).
    Ambiguous {
        /// What was typed.
        term: String,
        /// `(id, name)` of up to eight candidates, and how many there are in all.
        candidates: Vec<(String, String)>,
        /// Number of matches.
        total: usize,
    },
}

impl SelectError {
    /// The message for people (and the `candidates` list for `--json`).
    pub fn message(&self) -> String {
        match self {
            Self::NotFound(m) => m.clone(),
            Self::Ambiguous { term, candidates, total } => {
                let mut s = format!("`{term}` matches {total} torrents; be more specific or use an id:");
                for (id, name) in candidates {
                    s.push_str(&format!("\n  {}  {}", &id[..id.len().min(8)], name));
                }
                if *total > candidates.len() {
                    s.push_str(&format!("\n  ... and {} more", total - candidates.len()));
                }
                s
            }
        }
    }

    /// Structured detail for `--json`.
    pub fn data(&self) -> Option<serde_json::Value> {
        match self {
            Self::NotFound(_) => None,
            Self::Ambiguous { candidates, total, .. } => Some(json!({
                "total": total,
                "candidates": candidates.iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>(),
            })),
        }
    }
}

impl Selection {
    /// Take `--all/-a`, `--tracker`, `--state`, `--category` out of the options.
    pub fn take(o: &mut Opts) -> Result<Self, Usage> {
        Ok(Self {
            terms: Vec::new(),
            all: o.flag(&["--all", "-a"]),
            tracker: o.value(&["--tracker"])?,
            state: o.value(&["--state"])?,
            category: o.value(&["--category"])?,
        })
    }

    /// Add the positional terms.
    pub fn with_terms(mut self, terms: Vec<String>) -> Self {
        self.terms = terms;
        self
    }

    /// Any filter flag is set.
    pub fn has_filter(&self) -> bool {
        self.all || self.tracker.is_some() || self.state.is_some() || self.category.is_some()
    }

    /// Only `--all`, nothing narrower (lets `pause --all` use the engine's one-pass call).
    pub fn is_plain_all(&self) -> bool {
        self.all && self.terms.is_empty() && self.tracker.is_none() && self.state.is_none() && self.category.is_none()
    }

    /// Check the shape for a command named `what`.
    pub fn validate(&self, what: &str) -> Result<(), Usage> {
        if self.terms.is_empty() && !self.has_filter() {
            return Err(Usage(format!("{what} needs a torrent (id, id prefix or name; see `seedwell list`) or a filter: --all, --tracker X, --state S")));
        }
        if !self.terms.is_empty() && self.has_filter() {
            return Err(Usage(format!("{what}: name torrents or use filters (--all, --tracker, --state, --category), not both")));
        }
        Ok(())
    }

    /// The list filter for `--state`.
    pub fn list_filter(&self) -> Result<ListFilter, Usage> {
        match &self.state {
            None => Ok(ListFilter::All),
            Some(f) => parse_state(f),
        }
    }
}

/// `downloading`, `seeding`, ... into the protocol's filter.
pub fn parse_state(f: &str) -> Result<ListFilter, Usage> {
    serde_json::from_value(json!(f.to_ascii_lowercase().replace('_', "-"))).map_err(|_| {
        Usage(format!(
            "unknown state `{f}` (all, downloading, queued, seeding, low-power, paused, error, finished, recent, checking, metadata, awaiting-choice, pending)"
        ))
    })
}

fn is_hex_prefix(t: &str) -> bool {
    (6..=40).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The title, and the raw name next to it when they differ (two releases can share a title).
fn candidate(r: &TransferDto) -> String {
    if !r.title.is_empty() && r.title != r.name { format!("{}  [{}]", r.title, r.name) } else { r.name.clone() }
}

fn candidates(rows: &[&TransferDto]) -> Vec<(String, String)> {
    rows.iter().take(MAX_CANDIDATES).map(|r| (r.id.clone(), candidate(r))).collect()
}

fn shown(r: &TransferDto) -> &str {
    if r.title.is_empty() { &r.name } else { &r.title }
}

/// Resolve one term against the rows: the full id, an id prefix, then an exact name, then a name substring.
pub fn match_term<'a>(rows: &'a [TransferDto], term: &str) -> Result<&'a TransferDto, SelectError> {
    let t = term.trim();
    if t.is_empty() {
        return Err(SelectError::NotFound("empty torrent name".into()));
    }
    let lower = t.to_lowercase();
    if is_hex_prefix(t) {
        let by_id: Vec<&TransferDto> = rows.iter().filter(|r| r.id.starts_with(&lower)).collect();
        match by_id.as_slice() {
            [one] => return Ok(one),
            [] => {}
            many => return Err(SelectError::Ambiguous { term: term.to_owned(), candidates: candidates(many), total: many.len() }),
        }
    }
    let exact: Vec<&TransferDto> = rows.iter().filter(|r| shown(r).to_lowercase() == lower || r.name.to_lowercase() == lower).collect();
    if let [one] = exact.as_slice() {
        return Ok(one);
    }
    let sub: Vec<&TransferDto> = rows.iter().filter(|r| shown(r).to_lowercase().contains(&lower) || r.name.to_lowercase().contains(&lower)).collect();
    match sub.as_slice() {
        [] => Err(SelectError::NotFound(format!("no torrent matches `{term}` (see `seedwell list`)"))),
        [one] => Ok(one),
        many => Err(SelectError::Ambiguous { term: term.to_owned(), candidates: candidates(many), total: many.len() }),
    }
}

/// The error for a command that needs exactly one torrent when several were picked.
pub fn need_one(picked: &[TransferDto], what: &str) -> Result<(), SelectError> {
    if picked.len() <= 1 {
        return Ok(());
    }
    let refs: Vec<&TransferDto> = picked.iter().collect();
    Err(SelectError::Ambiguous { term: what.to_owned(), candidates: candidates(&refs), total: picked.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id_char: char, name: &str) -> TransferDto {
        TransferDto { id: id_char.to_string().repeat(40), name: name.into(), ..Default::default() }
    }

    fn rows() -> Vec<TransferDto> {
        vec![row('a', "Ubuntu 24.04 Desktop"), row('b', "Ubuntu 24.04 Server"), row('c', "Sintel (2010)")]
    }

    #[test]
    fn id_prefix_name_and_ambiguity() {
        let r = rows();
        assert_eq!(match_term(&r, "aaaaaa").unwrap().name, "Ubuntu 24.04 Desktop");
        assert_eq!(match_term(&r, "sintel").unwrap().name, "Sintel (2010)");
        assert_eq!(match_term(&r, "ubuntu 24.04 server").unwrap().id, "b".repeat(40));
        assert_eq!(match_term(&r, "desktop").unwrap().id, "a".repeat(40));
        let e = match_term(&r, "ubuntu").unwrap_err();
        match &e {
            SelectError::Ambiguous { total, candidates, .. } => {
                assert_eq!(*total, 2);
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("{other:?}"),
        }
        assert!(e.message().contains("matches 2 torrents") && e.message().contains("aaaaaaaa"), "{}", e.message());
        assert!(matches!(match_term(&r, "nothing").unwrap_err(), SelectError::NotFound(_)));
    }

    #[test]
    fn validate_shapes() {
        let mut o = Opts::new(vec!["--all".into()]);
        let s = Selection::take(&mut o).unwrap();
        assert!(s.is_plain_all() && s.validate("pause").is_ok());
        let s = Selection::default();
        assert!(s.validate("pause").is_err());
        let s = Selection { all: true, ..Default::default() }.with_terms(vec!["x".into()]);
        assert!(s.validate("pause").is_err());
    }

    #[test]
    fn states() {
        assert!(parse_state("seeding").is_ok());
        assert!(parse_state("awaiting_choice").is_ok());
        assert!(parse_state("nope").is_err());
    }
}

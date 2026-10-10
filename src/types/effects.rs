//! Effect rows (Koka-style): a set of `resource.operation` labels plus an optional row
//! variable that stands for "whatever further effects". A function type carries the row of
//! effects that happen when it is called.

use std::collections::BTreeSet;
use std::fmt;

pub type RowVarId = usize;

pub const DB_READ: (&str, &str) = ("db", "read");
pub const DB_WRITE: (&str, &str) = ("db", "write");
pub const SERVER_CALL: (&str, &str) = ("server", "call");

/// Labels of the v1 vocabulary. Caches, queues and files will add theirs when the language
/// gains operations on them.
const KNOWN: &[(&str, &str)] = &[DB_READ, DB_WRITE, SERVER_CALL];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Label {
    pub resource: String,
    pub operation: String,
}

// Label constructors and parsing (and through `is_known`, the vocabulary above) are consumed
// by effect annotations and domain ceilings, which build on this representation; until then
// only tests use them.
#[allow(dead_code)]
impl Label {
    pub fn new(resource: &str, operation: &str) -> Label {
        Label {
            resource: resource.to_string(),
            operation: operation.to_string(),
        }
    }

    pub fn of(pair: (&str, &str)) -> Label {
        Label::new(pair.0, pair.1)
    }

    /// An effect variable inside the body of a function annotated with `effects [e]`: it only
    /// equals itself and is exempt from domain ceilings (the caller decides what it becomes).
    pub fn rigid(name: &str) -> Label {
        Label {
            resource: format!("'{name}"),
            operation: String::new(),
        }
    }

    pub fn is_rigid(&self) -> bool {
        self.resource.starts_with('\'')
    }

    /// `resource.operation` with exactly one dot.
    pub fn parse(text: &str) -> Option<Label> {
        let (resource, operation) = text.split_once('.')?;
        if resource.is_empty() || operation.is_empty() || operation.contains('.') {
            return None;
        }
        Some(Label::new(resource, operation))
    }

    pub fn is_known(&self) -> bool {
        KNOWN.contains(&(self.resource.as_str(), self.operation.as_str()))
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_rigid() {
            write!(f, "{}", &self.resource[1..])
        } else {
            write!(f, "{}.{}", self.resource, self.operation)
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub labels: BTreeSet<Label>,
    pub tail: Option<RowVarId>,
}

impl Row {
    pub fn empty() -> Row {
        Row::default()
    }

    pub fn closed(labels: impl IntoIterator<Item = Label>) -> Row {
        Row {
            labels: labels.into_iter().collect(),
            tail: None,
        }
    }

    pub fn with_tail(labels: impl IntoIterator<Item = Label>, tail: RowVarId) -> Row {
        Row {
            labels: labels.into_iter().collect(),
            tail: Some(tail),
        }
    }

    pub fn is_pure(&self) -> bool {
        self.labels.is_empty() && self.tail.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    /// `ceiling`: the most labels this variable may ever stand for (domains), `None` = any.
    Unbound {
        level: u32,
        ceiling: Option<BTreeSet<Label>>,
    },
    Link(Row),
}

pub fn intersect(
    a: &Option<BTreeSet<Label>>,
    b: &Option<BTreeSet<Label>>,
) -> Option<BTreeSet<Label>> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.intersection(y).cloned().collect()),
        (Some(x), None) | (None, Some(x)) => Some(x.clone()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays_labels() {
        let label = Label::parse("db.read").unwrap();
        assert_eq!(label, Label::new("db", "read"));
        assert_eq!(label.to_string(), "db.read");
        assert!(label.is_known());
        assert!(!Label::new("db", "delete").is_known());
        assert_eq!(Label::parse("db"), None);
        assert_eq!(Label::parse("a.b.c"), None);
        let rigid = Label::rigid("e");
        assert!(rigid.is_rigid());
        assert_eq!(rigid.to_string(), "e");
    }

    #[test]
    fn intersects_ceilings() {
        let read: BTreeSet<Label> = [Label::new("db", "read")].into();
        let both: BTreeSet<Label> = [Label::new("db", "read"), Label::new("db", "write")].into();
        assert_eq!(
            intersect(&Some(both.clone()), &Some(read.clone())),
            Some(read.clone())
        );
        assert_eq!(intersect(&None, &Some(both.clone())), Some(both));
        assert_eq!(intersect(&None, &None), None);
    }
}

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub required: bool,
    pub has_default: bool,
}

#[derive(Clone, Debug)]
pub struct Table {
    pub name: String,
    pub model: String,
    pub primary_key: String,
    pub columns: BTreeMap<String, Column>,
}

#[derive(Clone, Debug, Default)]
pub struct DatabaseSchema {
    pub engine: String,
    pub tables: BTreeMap<String, Table>,
    pub fingerprint: String,
}

impl DatabaseSchema {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("Cannot read database schema {}: {error}", path.display()))?;
        let mut schema = DatabaseSchema {
            engine: "postgresql".into(),
            ..Default::default()
        };
        for (line_index, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || line == HEADER {
                continue;
            }
            let line_number = line_index + 1;
            let fields: Vec<_> = line.split(',').map(str::trim).collect();
            if fields.len() != 9 {
                return Err(format!(
                    "Invalid schema snapshot line {line_number}: expected 9 comma-separated fields ({HEADER})"
                ));
            }
            if let Some(index) = fields[..5].iter().position(|field| field.is_empty()) {
                return Err(format!(
                    "Invalid schema snapshot line {line_number}: field `{}` is empty",
                    HEADER.split(',').nth(index).unwrap_or_default()
                ));
            }
            let table_name = fields[0].to_string();
            let model = fields[1].to_string();
            let primary_key = fields[2].to_string();
            let column = Column {
                name: fields[3].into(),
                type_name: fields[4].into(),
                nullable: parse_bool(fields[5])?,
                required: parse_bool(fields[6])?,
                has_default: parse_bool(fields[7])?,
            };
            if fields[8] != "postgresql" {
                return Err(format!(
                    "Unsupported database engine `{}` on line {line_number}",
                    fields[8]
                ));
            }
            let table = schema
                .tables
                .entry(table_name.clone())
                .or_insert_with(|| Table {
                    name: table_name.clone(),
                    model: model.clone(),
                    primary_key: primary_key.clone(),
                    columns: BTreeMap::new(),
                });
            if table.model != model || table.primary_key != primary_key {
                return Err(format!(
                    "Schema snapshot line {line_number}: table `{table_name}` is declared with model `{model}` and primary key `{primary_key}`, but earlier lines say model `{}` and primary key `{}`",
                    table.model, table.primary_key
                ));
            }
            if table.columns.insert(column.name.clone(), column).is_some() {
                return Err(format!("Duplicate database column on line {line_number}"));
            }
        }
        // A primary key that is not a column is reported by the type checker (with the model's
        // location) for every table the program uses.
        schema.fingerprint = stable_hash(&schema.canonical());
        Ok(schema)
    }

    pub fn canonical(&self) -> String {
        let mut output = format!("engine:{}\n", self.engine);
        for table in self.tables.values() {
            output.push_str(&format!(
                "table:{}:{}:{}\n",
                escape(&table.name),
                escape(&table.model),
                escape(&table.primary_key)
            ));
            for column in table.columns.values() {
                output.push_str(&format!(
                    "column:{}:{}:{}:{}:{}\n",
                    escape(&column.name),
                    escape(&column.type_name),
                    column.nullable,
                    column.required,
                    column.has_default
                ));
            }
        }
        output
    }
}

/// The header line of a schema snapshot (skipped only when it matches exactly).
const HEADER: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine";

/// A canonical-form field with `\\`, `:` and newlines escaped, so distinct schemas never share a
/// canonical form (and a fingerprint).
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\n', "\\n")
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("Expected boolean, found `{value}`")),
    }
}

pub fn stable_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    format!("sha256:{digest:x}")
}

pub fn closest<'a>(needle: &str, candidates: impl Iterator<Item = &'a String>) -> Option<String> {
    candidates
        .map(|candidate| (distance(needle, candidate), candidate.clone()))
        .min_by_key(|item| item.0)
        .and_then(|(score, value)| if score <= 3 { Some(value) } else { None })
}

fn distance(a: &str, b: &str) -> usize {
    let mut costs: Vec<usize> = (0..=b.chars().count()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = costs[0];
        costs[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let old = costs[j + 1];
            costs[j + 1] = if ca == cb {
                previous
            } else {
                1 + previous.min(costs[j]).min(old)
            };
            previous = old;
        }
    }
    *costs.last().unwrap_or(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suggests_close_names() {
        assert_eq!(
            closest("fullName", ["name".into(), "full_name".into()].iter()),
            Some("full_name".into())
        );
    }

    fn load(text: &str) -> Result<DatabaseSchema, String> {
        let path = std::env::temp_dir().join(format!(
            "bindc-schema-{}-{:?}.csv",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::write(&path, text).unwrap();
        let schema = DatabaseSchema::load(&path);
        fs::remove_file(&path).unwrap();
        schema
    }

    #[test]
    fn conflicting_tables_missing_keys_and_bad_headers_are_errors() {
        let row = |table: &str, model: &str, key: &str, column: &str| {
            format!("{table},{model},{key},{column},String,false,true,false,postgresql\n")
        };
        let ok = format!(
            "{HEADER}\n{}{}",
            row("c", "C", "id", "id"),
            row("c", "C", "id", "name")
        );
        assert_eq!(load(&ok).unwrap().tables["c"].columns.len(), 2);
        for (text, expected) in [
            (
                format!(
                    "{}{}",
                    row("c", "C", "id", "id"),
                    row("c", "Other", "id", "name")
                ),
                "earlier lines say model `C`",
            ),
            (
                format!(
                    "{}{}",
                    row("c", "C", "id", "id"),
                    row("c", "C", "name", "name")
                ),
                "primary key",
            ),
            (row("c", "", "id", "id"), "field `model` is empty"),
            // A table literally named `table` is data, not a header.
            (
                format!(
                    "{}{}",
                    row("table", "T", "x", "x"),
                    row("table", "U", "x", "y")
                ),
                "earlier lines say model `T`",
            ),
            // Only the exact header line is skipped: a truncated header is an error.
            (
                "table,model,primary_key,column,type\n".to_string(),
                "expected 9 comma-separated fields",
            ),
        ] {
            let error = load(&text).unwrap_err();
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn the_canonical_form_escapes_its_separator() {
        let a = load("t,M,a:b,a:b,String,false,true,false,postgresql\n").unwrap();
        let b = load("t,M,a,a,b:String,false,true,false,postgresql\n");
        // Distinct schemas never share a fingerprint, whatever their names contain.
        if let Ok(b) = b {
            assert_ne!(a.fingerprint, b.fingerprint);
        }
        assert!(a.canonical().contains("table:t:M:a\\:b\n"));
    }
}

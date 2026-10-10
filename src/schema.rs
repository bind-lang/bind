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
            if line.is_empty() || line.starts_with('#') || line.starts_with("table,") {
                continue;
            }
            let fields: Vec<_> = line.split(',').map(str::trim).collect();
            if fields.len() != 9 {
                return Err(format!(
                    "Invalid schema snapshot line {}: expected 9 comma-separated fields",
                    line_index + 1
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
                    "Unsupported database engine `{}` on line {}",
                    fields[8],
                    line_index + 1
                ));
            }
            let table = schema
                .tables
                .entry(table_name.clone())
                .or_insert_with(|| Table {
                    name: table_name,
                    model,
                    primary_key,
                    columns: BTreeMap::new(),
                });
            if table.columns.insert(column.name.clone(), column).is_some() {
                return Err(format!(
                    "Duplicate database column on line {}",
                    line_index + 1
                ));
            }
        }
        schema.fingerprint = stable_hash(&schema.canonical());
        Ok(schema)
    }

    pub fn canonical(&self) -> String {
        let mut output = format!("engine:{}\n", self.engine);
        for table in self.tables.values() {
            output.push_str(&format!(
                "table:{}:{}:{}\n",
                table.name, table.model, table.primary_key
            ));
            for column in table.columns.values() {
                output.push_str(&format!(
                    "column:{}:{}:{}:{}:{}\n",
                    column.name,
                    column.type_name,
                    column.nullable,
                    column.required,
                    column.has_default
                ));
            }
        }
        output
    }
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
}

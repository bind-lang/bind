use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ProjectConfig {
    pub root: PathBuf,
    pub name: String,
    pub version: String,
    pub source: PathBuf,
    pub output: PathBuf,
    pub strict: bool,
    pub warnings_as_errors: bool,
    pub database_schema: Option<PathBuf>,
    pub database_url_env: Option<String>,
    pub database_verification: String,
    pub web_title: String,
    pub web_description: String,
    pub web_language: String,
    pub web_stylesheet: Option<PathBuf>,
    pub targets: BTreeMap<String, TargetConfig>,
}

#[derive(Clone, Debug, Default)]
pub struct TargetConfig {
    pub client: String,
    pub server: String,
    pub deployment: String,
    pub certified: bool,
    pub capabilities: Vec<String>,
}

impl ProjectConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
        let root = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let values = parse_toml_subset(&text)?;
        let get =
            |section: &str, key: &str| values.get(section).and_then(|map| map.get(key)).cloned();
        let source = get("project", "source").unwrap_or_else(|| "src".into());
        let output = get("project", "output").unwrap_or_else(|| ".bind/generated".into());
        let mut targets = BTreeMap::new();
        for (section, properties) in &values {
            if let Some(name) = section.strip_prefix("target.") {
                targets.insert(
                    name.into(),
                    TargetConfig {
                        client: properties.get("client").cloned().unwrap_or_default(),
                        server: properties.get("server").cloned().unwrap_or_default(),
                        deployment: properties
                            .get("deployment")
                            .cloned()
                            .unwrap_or_else(|| name.to_string()),
                        certified: parse_bool(properties.get("certified").cloned(), false),
                        capabilities: properties
                            .get("capabilities")
                            .map(|value| parse_array(value))
                            .unwrap_or_default(),
                    },
                );
            }
        }
        if targets.is_empty() {
            targets.insert(
                "aws-lambda".into(),
                TargetConfig {
                    client: "elm".into(),
                    server: "rust".into(),
                    deployment: "aws-lambda".into(),
                    certified: true,
                    capabilities: vec![
                        "ui.dom".into(),
                        "server.http".into(),
                        "db.read".into(),
                        "db.write".into(),
                    ],
                },
            );
        }
        Ok(Self {
            root: root.clone(),
            name: get("project", "name").unwrap_or_else(|| "bind-app".into()),
            version: get("project", "version").unwrap_or_else(|| "0.1.0".into()),
            source: root.join(source),
            output: root.join(output),
            strict: parse_bool(get("compiler", "strict"), true),
            warnings_as_errors: parse_bool(get("compiler", "warnings_as_errors"), false),
            database_schema: get("database.main", "schema").map(|value| root.join(value)),
            database_url_env: get("database.main", "env"),
            database_verification: get("database.main", "verification")
                .unwrap_or_else(|| "static".into()),
            web_title: get("web", "title").unwrap_or_else(|| "Bind application".into()),
            web_description: get("web", "description").unwrap_or_default(),
            web_language: get("web", "language").unwrap_or_else(|| "en".into()),
            web_stylesheet: get("web", "stylesheet").map(|value| root.join(value)),
            targets,
        })
    }
}

fn parse_toml_subset(text: &str) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    let mut result: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut section = String::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("Invalid bind.toml line {}: {line}", index + 1));
        };
        let value = value.trim();
        let decoded = if value.starts_with('"') && value.ends_with('"') {
            value[1..value.len() - 1].to_string()
        } else {
            value.to_string()
        };
        result
            .entry(section.clone())
            .or_default()
            .insert(key.trim().into(), decoded);
    }
    Ok(result)
}

fn parse_bool(value: Option<String>, fallback: bool) -> bool {
    value.map(|item| item == "true").unwrap_or(fallback)
}

fn parse_array(value: &str) -> Vec<String> {
    value
        .trim_matches(|ch| ch == '[' || ch == ']')
        .split(',')
        .map(|item| item.trim().trim_matches('"').to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections() {
        let values = parse_toml_subset(
            "[project]\nname = \"shop\"\n[target.vercel]\nclient = \"elm\"\ndeployment = \"vercel\"\ncertified = true",
        )
        .unwrap();
        assert_eq!(values["project"]["name"], "shop");
        assert_eq!(values["target.vercel"]["client"], "elm");
    }
}

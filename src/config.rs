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
    /// `mode = "ssr"` (the server renders the page) or `"split"` (static frontend calling the
    /// API); required for web targets (E8311 when absent).
    pub mode: Option<Mode>,
    /// Base URL of the RPC routes the frontend calls (`<api_url>/<endpoint>`); `/api` by default.
    pub api_url: String,
    /// The only browser origin allowed to call the API (CORS), required when `api_url` is on
    /// another origin.
    pub frontend_origin: Option<String>,
    pub certified: bool,
    pub capabilities: Vec<String>,
}

/// How a web target is deployed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    /// One application: the Rust server renders the page and serves the API.
    Ssr,
    /// Two applications: a static frontend (HTML, CSS, JS) calling the Rust API over HTTP.
    #[default]
    Split,
}

impl TargetConfig {
    /// The mode's name, `unset` when the target declares none.
    pub fn mode_name(&self) -> &'static str {
        self.mode.map_or("unset", Mode::as_str)
    }
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Ssr => "ssr",
            Mode::Split => "split",
        }
    }
}

/// The default base URL of the RPC routes (`api_url`).
pub const DEFAULT_API_URL: &str = "/api";

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
                let mode = match properties.get("mode").map(String::as_str) {
                    None => None,
                    Some("split") => Some(Mode::Split),
                    Some("ssr") => Some(Mode::Ssr),
                    Some(other) => {
                        return Err(format!(
                            "Target `{name}`: `mode` must be \"ssr\" or \"split\", found \"{other}\""
                        ));
                    }
                };
                targets.insert(
                    name.into(),
                    TargetConfig {
                        client: properties.get("client").cloned().unwrap_or_default(),
                        server: properties.get("server").cloned().unwrap_or_default(),
                        deployment: properties
                            .get("deployment")
                            .cloned()
                            .unwrap_or_else(|| name.to_string()),
                        mode,
                        api_url: properties
                            .get("api_url")
                            .cloned()
                            .unwrap_or_else(|| DEFAULT_API_URL.to_string()),
                        frontend_origin: properties.get("frontend_origin").cloned(),
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
                    client: "web".into(),
                    server: "rust".into(),
                    deployment: "aws-lambda".into(),
                    mode: Some(Mode::Split),
                    api_url: DEFAULT_API_URL.to_string(),
                    frontend_origin: None,
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
        let line = strip_comment(raw_line).trim();
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

/// `line` without its comment: from the first `#` that is outside a quoted string (`"…"`, where
/// `\` escapes the next character, or `'…'`) to the end of the line.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, c) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' => return &line[..index],
            None => {}
        }
    }
    line
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
    fn a_hash_inside_a_quoted_string_is_not_a_comment() {
        let values = parse_toml_subset(
            "[web]\ntitle = \"C# & F# tips\" # the title\ndescription = 'a#b' # c\nlanguage = \"en\\\"#\" # quote then hash\n# whole line\nstylesheet = \"s.css\"#x\n",
        )
        .unwrap();
        assert_eq!(values["web"]["title"], "C# & F# tips");
        assert_eq!(values["web"]["description"], "'a#b'");
        assert_eq!(values["web"]["language"], "en\\\"#");
        assert_eq!(values["web"]["stylesheet"], "s.css");
        assert_eq!(strip_comment("a = 1 # c"), "a = 1 ");
        assert_eq!(strip_comment("a = \"#\""), "a = \"#\"");
    }

    #[test]
    fn parses_sections() {
        let values = parse_toml_subset(
            "[project]\nname = \"shop\"\n[target.vercel]\nclient = \"web\"\ndeployment = \"vercel\"\ncertified = true",
        )
        .unwrap();
        assert_eq!(values["project"]["name"], "shop");
        assert_eq!(values["target.vercel"]["client"], "web");
    }

    fn load(text: &str) -> Result<ProjectConfig, String> {
        let root = std::env::temp_dir().join(format!(
            "bindc-config-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("bind.toml"), text).unwrap();
        let config = ProjectConfig::load(&root.join("bind.toml"));
        fs::remove_dir_all(&root).unwrap();
        config
    }

    #[test]
    fn web_targets_have_a_mode_an_api_url_and_an_optional_frontend_origin() {
        let config = load(
            "[target.a]\nclient = \"web\"\nmode = \"ssr\"\n[target.b]\nclient = \"web\"\nmode = \"split\"\napi_url = \"https://api.example.com/api\"\nfrontend_origin = \"https://app.example.com\"\n[target.c]\nclient = \"web\"\n",
        )
        .unwrap();
        let a = &config.targets["a"];
        assert_eq!((a.mode, a.api_url.as_str()), (Some(Mode::Ssr), "/api"));
        assert_eq!(a.frontend_origin, None);
        let b = &config.targets["b"];
        assert_eq!(b.mode, Some(Mode::Split));
        assert_eq!(b.api_url, "https://api.example.com/api");
        assert_eq!(
            b.frontend_origin.as_deref(),
            Some("https://app.example.com")
        );
        assert_eq!(config.targets["c"].mode, None);
        let error = load("[target.a]\nmode = \"spa\"\n").unwrap_err();
        assert!(
            error.contains("`mode` must be \"ssr\" or \"split\""),
            "{error}"
        );
    }
}

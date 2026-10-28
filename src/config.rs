use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ProjectConfig {
    pub root: PathBuf,
    /// The manifest file itself (`bind.toml`).
    pub manifest: PathBuf,
    pub name: String,
    pub version: String,
    pub source: PathBuf,
    /// The directory of the generated workspaces (`<output>/<target>`): always a strict
    /// subdirectory of `root` (checked by [`ProjectConfig::load`]).
    pub output: PathBuf,
    pub warnings_as_errors: bool,
    pub database_schema: Option<PathBuf>,
    pub database_url_env: Option<String>,
    pub database_verification: String,
    /// `[database.main].pooler`: what sits between the server and PostgreSQL.
    pub database_pooler: Pooler,
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

/// The connection pooler in front of PostgreSQL (`[database.main].pooler`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Pooler {
    /// `"session"`: a direct connection or a session-mode pooler; a connection is the
    /// server's for its lifetime, so sqlx caches prepared statements on it.
    #[default]
    Session,
    /// `"transaction"`: a transaction-mode pooler (PgBouncer, Supavisor, RDS Proxy…) that may
    /// hand each transaction to a different server connection; no prepared-statement cache.
    Transaction,
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

/// The platform a target deploys to (`deployment` in `bind.toml`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Deployment {
    AwsLambda,
    Vercel,
    CloudflareWorkers,
    /// A deployment no built-in generator supports (E8204).
    Other,
}

/// The generator that implements a target, derived from its client, server and deployment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    /// Web client + Rust server on AWS Lambda or Vercel (`rust-web-v4`).
    RustWeb(Deployment),
    /// Web client + Bind WebAssembly on Cloudflare Workers (`cloudflare-worker-wasm-v1`).
    CloudflareWasm,
    /// No built-in generator (E8204).
    Unsupported,
}

impl TargetConfig {
    /// The mode's name, `unset` when the target declares none.
    pub fn mode_name(&self) -> &'static str {
        self.mode.map_or("unset", Mode::as_str)
    }

    pub fn deployment_kind(&self) -> Deployment {
        match self.deployment.as_str() {
            "aws-lambda" => Deployment::AwsLambda,
            "vercel" => Deployment::Vercel,
            "cloudflare-workers" => Deployment::CloudflareWorkers,
            _ => Deployment::Other,
        }
    }

    pub fn backend(&self) -> Backend {
        match (
            self.client.as_str(),
            self.server.as_str(),
            self.deployment_kind(),
        ) {
            ("web", "rust", deployment @ (Deployment::AwsLambda | Deployment::Vercel)) => {
                Backend::RustWeb(deployment)
            }
            ("web", "wasm", Deployment::CloudflareWorkers) => Backend::CloudflareWasm,
            _ => Backend::Unsupported,
        }
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

/// Whether `name` may name a target: `[a-z0-9][a-z0-9_-]*`. Target names become directory and
/// file names (`<output>/<target>`, `.bind/locks/<target>.lock`), so nothing else is accepted.
pub fn valid_target_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

/// Whether `value` is a portable environment variable name (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn valid_environment_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// A value of the manifest's TOML subset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    String(String),
    Bool(bool),
    Array(Vec<String>),
}

impl Value {
    fn kind(&self) -> &'static str {
        match self {
            Value::String(_) => "a string",
            Value::Bool(_) => "a boolean",
            Value::Array(_) => "an array of strings",
        }
    }
}

/// The type a manifest key requires.
#[derive(Clone, Copy)]
enum Kind {
    String,
    Bool,
    Array,
}

const PROJECT_KEYS: &[(&str, Kind)] = &[
    ("name", Kind::String),
    ("version", Kind::String),
    ("source", Kind::String),
    ("output", Kind::String),
];
const COMPILER_KEYS: &[(&str, Kind)] = &[("warnings_as_errors", Kind::Bool)];
const DATABASE_KEYS: &[(&str, Kind)] = &[
    ("engine", Kind::String),
    ("schema", Kind::String),
    ("env", Kind::String),
    ("verification", Kind::String),
    ("pooler", Kind::String),
];
const WEB_KEYS: &[(&str, Kind)] = &[
    ("title", Kind::String),
    ("description", Kind::String),
    ("language", Kind::String),
    ("stylesheet", Kind::String),
];
const TARGET_KEYS: &[(&str, Kind)] = &[
    ("client", Kind::String),
    ("server", Kind::String),
    ("mode", Kind::String),
    ("deployment", Kind::String),
    ("api_url", Kind::String),
    ("frontend_origin", Kind::String),
    ("certified", Kind::Bool),
    ("capabilities", Kind::Array),
];

type Table = BTreeMap<String, Value>;
type Tables = BTreeMap<Vec<String>, Table>;

impl ProjectConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
        let root = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let fail = |error: String| format!("{}: {error}", path.display());
        let tables = parse_toml_subset(&text).map_err(fail)?;
        check_schema(&tables).map_err(fail)?;
        let section = |name: &str| tables.get(&vec![name.to_string()]);
        let database = tables.get(&vec!["database".to_string(), "main".to_string()]);
        let project = section("project");
        let source = string(project, "source").unwrap_or_else(|| "src".into());
        let output = string(project, "output").unwrap_or_else(|| ".bind/generated".into());
        check_output_setting(&output).map_err(fail)?;
        let database_url_env = string(database, "env");
        if let Some(variable) = &database_url_env
            && !valid_environment_name(variable)
        {
            return Err(fail(format!(
                "`[database.main].env` must be an environment variable name ([A-Za-z_][A-Za-z0-9_]*), found \"{variable}\""
            )));
        }
        if let Some(engine) = string(database, "engine")
            && engine != "postgresql"
        {
            return Err(fail(format!(
                "`[database.main].engine` must be \"postgresql\", found \"{engine}\""
            )));
        }
        let database_pooler = match string(database, "pooler").as_deref() {
            None | Some("session") => Pooler::Session,
            Some("transaction") => Pooler::Transaction,
            Some(other) => {
                return Err(fail(format!(
                    "`[database.main].pooler` must be \"session\" or \"transaction\", found \"{other}\""
                )));
            }
        };
        let mut targets = BTreeMap::new();
        for (key, properties) in &tables {
            let [first, name] = key.as_slice() else {
                continue;
            };
            if first != "target" {
                continue;
            }
            let table = Some(properties);
            let mode = match string(table, "mode").as_deref() {
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
                name.clone(),
                TargetConfig {
                    client: string(table, "client").unwrap_or_default(),
                    server: string(table, "server").unwrap_or_default(),
                    deployment: string(table, "deployment").unwrap_or_else(|| name.clone()),
                    mode,
                    api_url: string(table, "api_url")
                        .unwrap_or_else(|| DEFAULT_API_URL.to_string()),
                    frontend_origin: string(table, "frontend_origin"),
                    certified: boolean(table, "certified").unwrap_or(false),
                    capabilities: match properties.get("capabilities") {
                        Some(Value::Array(items)) => items.clone(),
                        _ => Vec::new(),
                    },
                },
            );
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
        let web = section("web");
        Ok(Self {
            root: root.clone(),
            manifest: path.to_path_buf(),
            name: string(project, "name").unwrap_or_else(|| "bind-app".into()),
            version: string(project, "version").unwrap_or_else(|| "0.1.0".into()),
            source: root.join(source),
            output: root.join(output),
            warnings_as_errors: boolean(section("compiler"), "warnings_as_errors").unwrap_or(false),
            database_schema: string(database, "schema").map(|value| root.join(value)),
            database_url_env,
            database_verification: string(database, "verification")
                .unwrap_or_else(|| "static".into()),
            database_pooler,
            web_title: string(web, "title").unwrap_or_else(|| "Bind application".into()),
            web_description: string(web, "description").unwrap_or_default(),
            web_language: string(web, "language").unwrap_or_else(|| "en".into()),
            web_stylesheet: string(web, "stylesheet").map(|value| root.join(value)),
            targets,
        })
    }

    /// The generated workspace of `target` (`<output>/<target>`).
    pub fn target_output(&self, target: &str) -> PathBuf {
        self.output.join(target)
    }

    /// The directory of the versioned target locks (`.bind/locks`).
    pub fn lock_directory(&self) -> PathBuf {
        self.root.join(".bind/locks")
    }
}

fn string(table: Option<&Table>, key: &str) -> Option<String> {
    match table.and_then(|table| table.get(key)) {
        Some(Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn boolean(table: Option<&Table>, key: &str) -> Option<bool> {
    match table.and_then(|table| table.get(key)) {
        Some(Value::Bool(value)) => Some(*value),
        _ => None,
    }
}

/// `[project].output` is replaced on every generation, so it must be a strict subdirectory of
/// the project: relative, without `..`, and not the project directory itself.
fn check_output_setting(output: &str) -> Result<(), String> {
    let mut normal = 0;
    for component in Path::new(output).components() {
        match component {
            Component::Normal(_) => normal += 1,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "`[project].output` must be a relative path inside the project without `..`, found \"{output}\""
                ));
            }
        }
    }
    if normal == 0 {
        return Err(format!(
            "`[project].output` must name a subdirectory of the project, found \"{output}\""
        ));
    }
    Ok(())
}

/// Every table is known, every key is known in its table and has the type it requires.
fn check_schema(tables: &Tables) -> Result<(), String> {
    for (path, properties) in tables {
        let name = path.join(".");
        let keys = match path.as_slice() {
            [single] if single == "project" => PROJECT_KEYS,
            [single] if single == "compiler" => COMPILER_KEYS,
            [single] if single == "web" => WEB_KEYS,
            [first, second] if first == "database" && second == "main" => DATABASE_KEYS,
            [first, target] if first == "target" => {
                if !valid_target_name(target) {
                    return Err(format!(
                        "invalid target name `{target}`: target names match [a-z0-9][a-z0-9_-]*"
                    ));
                }
                TARGET_KEYS
            }
            _ => {
                return Err(format!(
                    "unknown table `[{name}]` (expected [project], [compiler], [web], [database.main] or [target.<name>])"
                ));
            }
        };
        for (key, value) in properties {
            let Some((_, kind)) = keys.iter().find(|(known, _)| known == key) else {
                return Err(format!(
                    "unknown key `{key}` in `[{name}]` (known keys: {})",
                    keys.iter()
                        .map(|(known, _)| *known)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            };
            let matches = matches!(
                (kind, value),
                (Kind::String, Value::String(_))
                    | (Kind::Bool, Value::Bool(_))
                    | (Kind::Array, Value::Array(_))
            );
            if !matches {
                let expected = match kind {
                    Kind::String => "a string",
                    Kind::Bool => "a boolean (true or false)",
                    Kind::Array => "an array of strings",
                };
                return Err(format!(
                    "`{key}` in `[{name}]` must be {expected}, found {}",
                    value.kind()
                ));
            }
        }
    }
    Ok(())
}

/// The TOML subset of `bind.toml`: `[table.subtable]` headers (bare or quoted keys), and
/// `key = value` pairs whose value is a basic string (with TOML escapes), a literal string, a
/// boolean or an array of strings (which may span lines). Anything else is an error: duplicate
/// tables or keys, keys outside a table, arrays of tables, inline tables, numbers, dates.
fn parse_toml_subset(text: &str) -> Result<Tables, String> {
    let mut tables: Tables = BTreeMap::new();
    let mut current: Option<Vec<String>> = None;
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line_number = index + 1;
        let error = |message: String| format!("line {line_number}: {message}");
        let mut logical = lines[index].to_string();
        index += 1;
        let mut cursor = Cursor::new(&logical);
        cursor.skip_whitespace();
        if cursor.at_end_or_comment() {
            continue;
        }
        if cursor.peek() == Some('[') {
            cursor.advance();
            if cursor.peek() == Some('[') {
                return Err(error(
                    "arrays of tables (`[[...]]`) are not supported".into(),
                ));
            }
            let path = cursor.dotted_key().map_err(error)?;
            cursor.skip_whitespace();
            if cursor.peek() != Some(']') {
                return Err(error("expected `]` after the table name".into()));
            }
            cursor.advance();
            cursor.expect_line_end().map_err(error)?;
            if tables.contains_key(&path) {
                return Err(error(format!(
                    "table `[{}]` is declared twice",
                    path.join(".")
                )));
            }
            tables.insert(path.clone(), BTreeMap::new());
            current = Some(path);
            continue;
        }
        let Some(table) = current.clone() else {
            return Err(error(
                "keys must be inside a table such as `[project]`".into(),
            ));
        };
        let key = cursor.key().map_err(error)?;
        cursor.skip_whitespace();
        if cursor.peek() == Some('.') {
            return Err(error(
                "dotted keys are not supported; use a table header".into(),
            ));
        }
        if cursor.peek() != Some('=') {
            return Err(error(format!("expected `=` after `{key}`")));
        }
        cursor.advance();
        cursor.skip_whitespace();
        // An array may continue on the next lines: gather lines until its `]`.
        if cursor.peek() == Some('[') {
            let start = cursor.position();
            loop {
                let mut probe = Cursor::new(&logical);
                probe.seek(start);
                match probe.array() {
                    Ok(_) => break,
                    Err(message) if message == UNTERMINATED_ARRAY && index < lines.len() => {
                        logical.push('\n');
                        logical.push_str(lines[index]);
                        index += 1;
                    }
                    Err(message) => return Err(error(message)),
                }
            }
            cursor = Cursor::new(&logical);
            cursor.seek(start);
        }
        let value = cursor.value().map_err(error)?;
        cursor.expect_line_end().map_err(error)?;
        let properties = tables.entry(table.clone()).or_default();
        if properties.contains_key(&key) {
            return Err(error(format!(
                "key `{key}` is declared twice in `[{}]`",
                table.join(".")
            )));
        }
        properties.insert(key, value);
    }
    Ok(tables)
}

const UNTERMINATED_ARRAY: &str = "unterminated array";

struct Cursor {
    chars: Vec<char>,
    index: usize,
}

impl Cursor {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            index: 0,
        }
    }

    fn position(&self) -> usize {
        self.index
    }

    fn seek(&mut self, index: usize) {
        self.index = index;
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek();
        self.index += 1;
        ch
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.index += 1;
        }
    }

    /// Whitespace, newlines and comments (inside arrays).
    fn skip_blank(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\n' | '\r') => self.index += 1,
                Some('#') => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.index += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn at_end_or_comment(&self) -> bool {
        matches!(self.peek(), None | Some('#'))
    }

    fn expect_line_end(&mut self) -> Result<(), String> {
        self.skip_whitespace();
        if self.at_end_or_comment() {
            Ok(())
        } else {
            Err(format!(
                "unexpected `{}` after the value",
                self.chars[self.index..].iter().collect::<String>().trim()
            ))
        }
    }

    /// A bare key (`A-Za-z0-9_-`) or a quoted one.
    fn key(&mut self) -> Result<String, String> {
        match self.peek() {
            Some('"') => self.basic_string(),
            Some('\'') => self.literal_string(),
            _ => {
                let start = self.index;
                while self
                    .peek()
                    .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
                {
                    self.index += 1;
                }
                if start == self.index {
                    return Err("expected a key".into());
                }
                Ok(self.chars[start..self.index].iter().collect())
            }
        }
    }

    fn dotted_key(&mut self) -> Result<Vec<String>, String> {
        let mut parts = Vec::new();
        loop {
            self.skip_whitespace();
            parts.push(self.key()?);
            self.skip_whitespace();
            if self.peek() == Some('.') {
                self.advance();
            } else {
                return Ok(parts);
            }
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some('"') => self.basic_string().map(Value::String),
            Some('\'') => self.literal_string().map(Value::String),
            Some('[') => self.array().map(Value::Array),
            _ => {
                let start = self.index;
                while self
                    .peek()
                    .is_some_and(|ch| !matches!(ch, ' ' | '\t' | '#'))
                {
                    self.index += 1;
                }
                let word: String = self.chars[start..self.index].iter().collect();
                match word.as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "" => Err("expected a value".into()),
                    other => Err(format!(
                        "unsupported value `{other}` (expected a quoted string, true, false or an array of strings)"
                    )),
                }
            }
        }
    }

    fn array(&mut self) -> Result<Vec<String>, String> {
        self.advance();
        let mut items = Vec::new();
        loop {
            self.skip_blank();
            match self.peek() {
                Some(']') => {
                    self.advance();
                    return Ok(items);
                }
                Some('"') => items.push(self.basic_string()?),
                Some('\'') => items.push(self.literal_string()?),
                None => return Err(UNTERMINATED_ARRAY.into()),
                Some(other) => {
                    return Err(format!("arrays may only hold strings, found `{other}`"));
                }
            }
            self.skip_blank();
            match self.peek() {
                Some(',') => {
                    self.advance();
                }
                Some(']') => {}
                None => return Err(UNTERMINATED_ARRAY.into()),
                Some(other) => {
                    return Err(format!("expected `,` or `]` in array, found `{other}`"));
                }
            }
        }
    }

    fn basic_string(&mut self) -> Result<String, String> {
        self.advance();
        let mut output = String::new();
        loop {
            match self.advance() {
                None | Some('\n') => return Err("unterminated string".into()),
                Some('"') => return Ok(output),
                Some('\\') => match self.advance() {
                    Some('"') => output.push('"'),
                    Some('\\') => output.push('\\'),
                    Some('n') => output.push('\n'),
                    Some('t') => output.push('\t'),
                    Some('r') => output.push('\r'),
                    Some('b') => output.push('\u{8}'),
                    Some('f') => output.push('\u{c}'),
                    Some('e') => output.push('\u{1b}'),
                    Some(kind @ ('u' | 'U')) => {
                        let length = if kind == 'u' { 4 } else { 8 };
                        let digits: String = (0..length).filter_map(|_| self.advance()).collect();
                        let ch = (digits.len() == length
                            && digits.chars().all(|ch| ch.is_ascii_hexdigit()))
                        .then(|| u32::from_str_radix(&digits, 16).ok())
                        .flatten()
                        .and_then(char::from_u32)
                        .ok_or_else(|| format!("invalid escape `\\{kind}{digits}`"))?;
                        output.push(ch);
                    }
                    Some(other) => return Err(format!("invalid escape `\\{other}`")),
                    None => return Err("unterminated string".into()),
                },
                Some(ch) if ch.is_control() && ch != '\t' => {
                    return Err(format!("control character U+{:04X} in a string", ch as u32));
                }
                Some(ch) => output.push(ch),
            }
        }
    }

    fn literal_string(&mut self) -> Result<String, String> {
        self.advance();
        let mut output = String::new();
        loop {
            match self.advance() {
                None | Some('\n') => return Err("unterminated string".into()),
                Some('\'') => return Ok(output),
                Some(ch) if ch.is_control() && ch != '\t' => {
                    return Err(format!("control character U+{:04X} in a string", ch as u32));
                }
                Some(ch) => output.push(ch),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table<'a>(tables: &'a Tables, path: &str) -> &'a Table {
        let key: Vec<String> = path.split('.').map(str::to_string).collect();
        &tables[&key]
    }

    fn text(value: &Value) -> &str {
        match value {
            Value::String(text) => text,
            other => panic!("not a string: {other:?}"),
        }
    }

    #[test]
    fn a_hash_inside_a_quoted_string_is_not_a_comment() {
        let values = parse_toml_subset(
            "[web]\ntitle = \"C# & F# tips\" # the title\ndescription = 'a#b' # c\nlanguage = \"en\\\"#\" # quote then hash\n# whole line\nstylesheet = \"s.css\"#x\n",
        )
        .unwrap();
        let web = table(&values, "web");
        assert_eq!(text(&web["title"]), "C# & F# tips");
        assert_eq!(text(&web["description"]), "a#b");
        assert_eq!(text(&web["language"]), "en\"#");
        assert_eq!(text(&web["stylesheet"]), "s.css");
    }

    #[test]
    fn parses_sections() {
        let values = parse_toml_subset(
            "[project]\nname = \"shop\"\n[target.vercel]\nclient = \"web\"\ndeployment = \"vercel\"\ncertified = true",
        )
        .unwrap();
        assert_eq!(text(&table(&values, "project")["name"]), "shop");
        assert_eq!(text(&table(&values, "target.vercel")["client"]), "web");
        assert_eq!(
            table(&values, "target.vercel")["certified"],
            Value::Bool(true)
        );
    }

    #[test]
    fn strings_decode_escapes_and_quoted_keys_lose_their_quotes() {
        let values = parse_toml_subset(
            "[web]\ntitle = \"Caf\\u00e9 \\\"q\\\" \\U0001F600\\t\\\\\"\n\"description\" = 'C:\\path'\n[\"target\".'x']\nclient = \"web\"\n",
        )
        .unwrap();
        let web = table(&values, "web");
        assert_eq!(text(&web["title"]), "Café \"q\" \u{1F600}\t\\");
        assert_eq!(text(&web["description"]), "C:\\path");
        assert!(values.contains_key(&vec!["target".to_string(), "x".to_string()]));
        for bad in ["\"a\\q\"", "\"\\u12\"", "\"open", "'open", "\"\\uD800\""] {
            assert!(
                parse_toml_subset(&format!("[web]\ntitle = {bad}\n")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn arrays_split_on_commas_outside_strings_and_may_span_lines() {
        let values = parse_toml_subset(
            "[target.a]\ncapabilities = [\"a,b\", 'c' , \"d]\" ]\n[target.b]\ncapabilities = [\n  \"x\", # first\n  \"y\",\n]\nclient = \"web\"\n",
        )
        .unwrap();
        assert_eq!(
            table(&values, "target.a")["capabilities"],
            Value::Array(vec!["a,b".into(), "c".into(), "d]".into()])
        );
        assert_eq!(
            table(&values, "target.b")["capabilities"],
            Value::Array(vec!["x".into(), "y".into()])
        );
        assert_eq!(text(&table(&values, "target.b")["client"]), "web");
        assert!(parse_toml_subset("[target.a]\ncapabilities = [1]\n").is_err());
        assert!(parse_toml_subset("[target.a]\ncapabilities = [\"a\" \"b\"]\n").is_err());
        assert!(parse_toml_subset("[target.a]\ncapabilities = [\"a\",\n").is_err());
    }

    #[test]
    fn duplicates_stray_keys_and_unsupported_syntax_are_errors() {
        for (manifest, expected) in [
            ("[project]\nname = \"a\"\nname = \"b\"\n", "declared twice"),
            ("[project]\n[project]\n", "declared twice"),
            (
                "[target.a]\nclient = \"web\"\n[target.a]\nmode = \"ssr\"\n",
                "declared twice",
            ),
            ("name = \"a\"\n", "inside a table"),
            ("[[target]]\n", "arrays of tables"),
            ("[project]\nname = 1\n", "unsupported value"),
            ("[project]\nname = \"a\" \"b\"\n", "unexpected"),
            ("[project]\na.b = \"c\"\n", "dotted keys"),
            ("[project]\nname = { a = 1 }\n", "unsupported value"),
            ("[project\n", "expected `]`"),
        ] {
            let error = parse_toml_subset(manifest).unwrap_err();
            assert!(error.contains(expected), "{manifest:?}: {error}");
        }
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
    fn the_database_pooler_is_session_unless_transaction_is_chosen() {
        assert_eq!(load("").unwrap().database_pooler, Pooler::Session);
        for (value, pooler) in [
            ("session", Pooler::Session),
            ("transaction", Pooler::Transaction),
        ] {
            let manifest =
                format!("[database.main]\nengine = \"postgresql\"\npooler = \"{value}\"\n");
            assert_eq!(load(&manifest).unwrap().database_pooler, pooler, "{value}");
        }
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

    #[test]
    fn unknown_tables_keys_and_mistyped_values_are_errors() {
        for (manifest, expected) in [
            // A typo must not silently turn certification off.
            ("[target.a]\ncertifed = true\n", "unknown key `certifed`"),
            ("[target.a]\ncertified = tru\n", "unsupported value"),
            ("[target.a]\ncertified = \"true\"\n", "must be a boolean"),
            ("[databse.main]\nschema = \"s.csv\"\n", "unknown table"),
            ("[project]\nname = [\"a\"]\n", "must be a string"),
            (
                "[target.a]\ncapabilities = \"ui.dom\"\n",
                "must be an array",
            ),
            (
                "[database.main]\nenv = \"DATABASE URL\"\n",
                "environment variable name",
            ),
            ("[database.main]\nengine = \"mysql\"\n", "postgresql"),
            (
                "[database.main]\npooler = \"statement\"\n",
                "`[database.main].pooler` must be \"session\" or \"transaction\"",
            ),
            ("[database.main]\npooler = true\n", "must be a string"),
        ] {
            let error = load(manifest).unwrap_err();
            assert!(error.contains(expected), "{manifest:?}: {error}");
        }
    }

    #[test]
    fn target_names_and_the_output_directory_cannot_leave_the_project() {
        for name in [
            "..",
            "../victim",
            "../../victim",
            "",
            "A",
            "-a",
            "a/b",
            "a.b",
            "\"quoted\"",
        ] {
            let manifest = format!(
                "[target.\"{}\"]\nclient = \"web\"\n",
                name.replace('"', "\\\"")
            );
            assert!(load(&manifest).is_err(), "{name:?}");
        }
        for name in ["a", "aws-lambda-ssr", "v2_split", "0"] {
            assert!(
                load(&format!("[target.{name}]\nclient = \"web\"\n")).is_ok(),
                "{name}"
            );
        }
        for output in [".", "", "./", "..", "../out", "/tmp/out", "out/../.."] {
            let error = load(&format!("[project]\noutput = \"{output}\"\n")).unwrap_err();
            assert!(error.contains("`[project].output`"), "{output}: {error}");
        }
        let config = load("[project]\noutput = \"build/gen\"\n").unwrap();
        assert!(config.output.ends_with("build/gen"));
    }

    #[test]
    fn deployments_and_backends_are_typed() {
        let config = load(
            "[target.a]\nclient = \"web\"\nserver = \"rust\"\ndeployment = \"vercel\"\n[target.b]\nclient = \"web\"\nserver = \"wasm\"\ndeployment = \"cloudflare-workers\"\n[target.c]\nclient = \"web\"\nserver = \"rust\"\ndeployment = \"fly\"\n",
        )
        .unwrap();
        assert_eq!(
            config.targets["a"].backend(),
            Backend::RustWeb(Deployment::Vercel)
        );
        assert_eq!(config.targets["b"].backend(), Backend::CloudflareWasm);
        assert_eq!(config.targets["c"].backend(), Backend::Unsupported);
        assert_eq!(config.targets["c"].deployment_kind(), Deployment::Other);
    }
}

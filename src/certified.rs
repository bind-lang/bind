use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::{Domain, TypeDecl, TypeKind, TypeRef};
use crate::compiler::{VerifiedApplication, json, json_array};
use crate::config::{ProjectConfig, TargetConfig};
use crate::diagnostic::Span;
use crate::lexer::{Token, TokenKind};
use crate::parser::expr::parse_type_text;
use crate::syntax::expr::TypeExpr;

const PROFILE_VERSION: &str = "rust-elm-web-v2";
/// JSON contract of the profile: `Int`, `Decimal` and `Money` travel as strings.
const WIRE_VERSION: &str = "bind-json-v2";
// Dependencies of the generated server, pinned with `=`. They match the runtime's
// `runtime/bind-runtime/Cargo.toml`, whose source the generated crate compiles.
const SERDE: &str = "1.0.229";
const SERDE_JSON: &str = "1.0.151";
const TOKIO: &str = "1.53.1";
const UUID: &str = "1.28.0";
const SQLX: &str = "0.9.0";
const NUM_BIGINT: &str = "0.4.6";
const BIGDECIMAL: &str = "0.4.8";
const RUST_DECIMAL: &str = "1.37.2";
const CHRONO: &str = "0.4.41";
const FUTURES: &str = "0.3.31";
/// The runtime of the generated server, written to its `src/bind_runtime.rs`.
const RUNTIME_SOURCE: &str = include_str!("../runtime/bind-runtime/src/lib.rs");
const LAMBDA_HTTP: &str = "1.3.1";
const VERCEL_RUNTIME: &str = "2.4.0";
const HTTP_BODY_UTIL: &str = "0.1.5";

#[derive(Clone, Debug)]
struct ComponentPlan {
    component: String,
    binding: Option<String>,
    procedure: Option<String>,
    text_expressions: Vec<Vec<Token>>,
    ui_nodes: Vec<UiNode>,
}

#[derive(Clone, Debug)]
struct UiNode {
    name: String,
    arguments: Vec<UiArgument>,
    children: Vec<UiNode>,
}

#[derive(Clone, Debug)]
enum UiArgument {
    String(String),
    Number(String),
}

pub fn generate(
    app: &VerifiedApplication,
    config: &ProjectConfig,
    output_override: Option<&Path>,
) -> Result<PathBuf, String> {
    let target_config = config
        .targets
        .get(&app.target)
        .ok_or_else(|| format!("Target `{}` disappeared from bind.toml", app.target))?;
    if target_config.client != "elm" || target_config.server != "rust" {
        return Err(format!(
            "Target `{}` is not the certified Rust + Elm profile",
            app.target
        ));
    }
    if !matches!(target_config.deployment.as_str(), "aws-lambda" | "vercel") {
        return Err(format!(
            "Unsupported certified deployment `{}`",
            target_config.deployment
        ));
    }

    let target = output_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config.output.join(&app.target));
    // A lock resolved for another profile's dependencies is re-resolved instead of kept.
    let same_profile = fs::read_to_string(target.join("bind-manifest.json"))
        .is_ok_and(|manifest| manifest.contains(&format!("\"profile\": \"{PROFILE_VERSION}\"")));
    let preserved_cargo_lock = fs::read_to_string(target.join("Cargo.lock"))
        .ok()
        .filter(|_| same_profile);
    let preserved_npm_lock = fs::read_to_string(target.join("package-lock.json")).ok();
    // Everything that can fail is computed first: a failure leaves the previous output intact.
    let mut files = workspace_files(app, config, target_config)?;
    if let Some(lock) = preserved_cargo_lock {
        files.push(("Cargo.lock".to_string(), lock));
    }
    if target_config.deployment == "vercel"
        && let Some(lock) = preserved_npm_lock
    {
        files.push(("package-lock.json".to_string(), lock));
    }
    if target.exists() {
        fs::remove_dir_all(&target)
            .map_err(|error| format!("Cannot replace {}: {error}", target.display()))?;
    }
    for directory in ["src", "api", "web/src", "web/dist", "contracts"] {
        fs::create_dir_all(target.join(directory)).map_err(|error| error.to_string())?;
    }
    for (path, content) in &files {
        write(&target.join(path), content)?;
    }
    Ok(target)
}

/// Every generated file (relative path, content) except the preserved lock files.
fn workspace_files(
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target_config: &TargetConfig,
) -> Result<Vec<(String, String)>, String> {
    let mut files: Vec<(String, String)> = Vec::new();
    let mut add = |path: &str, content: String| files.push((path.to_string(), content));
    let component = component_plan(app)?;
    add("Cargo.toml", cargo_toml(app, target_config));
    if app.rpc_contracts.is_empty() {
        add("src/lib.rs", STATIC_LIBRARY.to_string());
    } else {
        add("src/lib.rs", crate::backend::rust::server_library(app)?);
        add("src/bind_runtime.rs", RUNTIME_SOURCE.to_string());
    }
    let database_environment = config.database_url_env.as_deref().unwrap_or("DATABASE_URL");
    match target_config.deployment.as_str() {
        "aws-lambda" => {
            add("src/main.rs", aws_main(database_environment));
            add("template.yaml", aws_template(app, database_environment));
        }
        "vercel" => {
            for rpc in &app.rpc_contracts {
                add(
                    &format!("api/{}.rs", rpc.name),
                    vercel_main(&rpc.path, database_environment),
                );
            }
            add("vercel.json", VERCEL_JSON.to_string());
            add("package.json", VERCEL_PACKAGE_JSON.to_string());
        }
        other => return Err(format!("Unsupported certified deployment `{other}`")),
    }
    add("web/elm.json", ELM_JSON.to_string());
    add("web/src/Main.elm", elm_main(app, &component)?);
    add("web/index.html", index_html(config));
    let styles = match &config.web_stylesheet {
        Some(path) => fs::read_to_string(path)
            .map_err(|error| format!("Cannot read web stylesheet {}: {error}", path.display()))?,
        None => WEB_CSS.to_string(),
    };
    add("web/styles.css", styles);
    add("contracts/schema.json", contract_schema(app));
    add("contracts/vectors.json", contract_vectors(app));
    add("bind-manifest.json", generated_manifest(app, target_config));
    add("README.md", generated_readme(target_config));
    Ok(files)
}

pub fn profile_version() -> &'static str {
    PROFILE_VERSION
}

pub fn wire_version() -> &'static str {
    WIRE_VERSION
}

pub fn dependency_lock(
    target: &TargetConfig,
    cargo_lock_hash: Option<&str>,
    npm_lock_hash: Option<&str>,
    elm_json_hash: Option<&str>,
) -> String {
    let dependencies = locked_dependencies();
    let cargo_lock = cargo_lock_hash.unwrap_or("unresolved");
    let npm_lock = npm_lock_hash.unwrap_or("not-applicable");
    let elm_json = elm_json_hash.unwrap_or("unresolved");
    format!(
        "# Generated by bindc. Do not edit.\n\n[bind]\nlanguage = \"0.1.0\"\nprofile = \"{PROFILE_VERSION}\"\n\n[target]\nclient = \"elm\"\nserver = \"rust\"\ndeployment = \"{}\"\n\n[toolchain]\nelm = \"0.19.2\"\nrust = \"1.98.0\"\nrustEdition = \"2024\"\n\n{dependencies}\n[resolution]\ncargoLockSha256 = \"{cargo_lock}\"\nnpmLockSha256 = \"{npm_lock}\"\nelmJsonSha256 = \"{elm_json}\"\n",
        target.deployment
    )
}

pub fn workspace_lock(config: &ProjectConfig) -> String {
    let targets = config
        .targets
        .iter()
        .map(|(name, target)| {
            format!(
                "[targets.{name}]\nclient = \"{}\"\nserver = \"{}\"\ndeployment = \"{}\"\ncertified = {}\n",
                target.client, target.server, target.deployment, target.certified
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let dependencies = locked_dependencies();
    format!(
        "# Generated by bindc. Do not edit.\n\n[bind]\nlanguage = \"0.1.0\"\nprofile = \"{PROFILE_VERSION}\"\n\n[toolchain]\nelm = \"0.19.2\"\nrust = \"1.98.0\"\nrustEdition = \"2024\"\n\n{dependencies}\n{targets}"
    )
}

/// The `[dependencies]` table of `bind.lock` and the target locks: every pinned version.
fn locked_dependencies() -> String {
    format!(
        "[dependencies]\nserde = \"{SERDE}\"\nserdeJson = \"{SERDE_JSON}\"\ntokio = \"{TOKIO}\"\nuuid = \"{UUID}\"\nsqlx = \"{SQLX}\"\nnumBigint = \"{NUM_BIGINT}\"\nbigdecimal = \"{BIGDECIMAL}\"\nrustDecimal = \"{RUST_DECIMAL}\"\nchrono = \"{CHRONO}\"\nfutures = \"{FUTURES}\"\nlambdaHttp = \"{LAMBDA_HTTP}\"\nvercelRuntime = \"{VERCEL_RUNTIME}\"\nhttpBodyUtil = \"{HTTP_BODY_UTIL}\"\n"
    )
}

/// The `[dependencies]` of a generated server crate shared by every deployment: the
/// runtime's dependencies (same versions and features) plus serde's `rc` (lists are
/// `Arc<Vec<T>>`) and `uuid` with `serde` (generated types hold `Uuid`s).
pub(crate) fn server_dependencies() -> String {
    format!(
        "serde = {{ version = \"={SERDE}\", features = [\"derive\", \"rc\"] }}\nserde_json = \"={SERDE_JSON}\"\ntokio = {{ version = \"={TOKIO}\", features = [\"rt-multi-thread\", \"macros\", \"sync\"] }}\nsqlx = {{ version = \"={SQLX}\", default-features = false, features = [\"runtime-tokio\", \"tls-rustls\", \"postgres\", \"uuid\", \"chrono\", \"bigdecimal\", \"rust_decimal\"] }}\nuuid = {{ version = \"={UUID}\", features = [\"serde\"] }}\nnum-bigint = \"={NUM_BIGINT}\"\nbigdecimal = \"={BIGDECIMAL}\"\nrust_decimal = {{ version = \"={RUST_DECIMAL}\", features = [\"serde-str\"] }}\nchrono = {{ version = \"={CHRONO}\", features = [\"serde\"] }}\nfutures = \"={FUTURES}\"\n"
    )
}

fn write(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|error| format!("Cannot write {}: {error}", path.display()))
}

fn cargo_toml(app: &VerifiedApplication, target: &TargetConfig) -> String {
    let package = package_name(&app.project_name);
    if app.rpc_contracts.is_empty() {
        return format!(
            "[package]\nname = \"{package}\"\nversion = \"{}\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\n",
            app.project_version
        );
    }
    let mut bins = String::new();
    let platform_dependency = if target.deployment == "aws-lambda" {
        bins.push_str("[[bin]]\nname = \"bootstrap\"\npath = \"src/main.rs\"\n");
        format!(
            "lambda_http = {{ version = \"={LAMBDA_HTTP}\", default-features = false, features = [\"apigw_http\"] }}\n"
        )
    } else {
        for rpc in &app.rpc_contracts {
            bins.push_str(&format!(
                "[[bin]]\nname = \"{}\"\npath = \"api/{}.rs\"\n\n",
                snake_case(&rpc.name),
                rpc.name
            ));
        }
        format!("vercel_runtime = \"={VERCEL_RUNTIME}\"\nhttp-body-util = \"={HTTP_BODY_UTIL}\"\n")
    };
    format!(
        "[package]\nname = \"{package}\"\nversion = \"{}\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\n{}{platform_dependency}\n{bins}\n[profile.release]\nstrip = true\nlto = \"fat\"\ncodegen-units = 1\npanic = \"abort\"\n",
        app.project_version,
        server_dependencies()
    )
}

fn component_plan(app: &VerifiedApplication) -> Result<ComponentPlan, String> {
    let component = app
        .program
        .functions
        .iter()
        .find(|function| function.domain == Domain::Client && function.component)
        .ok_or_else(|| "Certified web target requires one client component".to_string())?;
    let mut binding = None;
    let mut procedure = None;
    for index in 0..component.body.len().saturating_sub(5) {
        if matches!(component.body[index].text.as_str(), "val" | "var")
            && component.body[index + 1].kind == TokenKind::Identifier
            && component.body[index + 2].text == "="
            && component.body[index + 3].text == "server"
            && component.body[index + 4].text == "."
        {
            binding = Some(component.body[index + 1].text.clone());
            procedure = Some(component.body[index + 5].text.clone());
            break;
        }
    }
    let ui_index = component
        .body
        .iter()
        .position(|token| token.text == "UI")
        .ok_or_else(|| format!("Component `{}` has no UI block", component.name))?;
    let open = component
        .body
        .iter()
        .enumerate()
        .skip(ui_index + 1)
        .find_map(|(index, token)| (token.text == "{").then_some(index))
        .ok_or_else(|| format!("Component `{}` has an invalid UI block", component.name))?;
    let mut expressions = Vec::new();
    let mut ui_nodes = Vec::new();
    if binding.is_some() {
        let mut index = open + 1;
        while index < component.body.len() {
            if component.body[index].text == "Text"
                && component
                    .body
                    .get(index + 1)
                    .is_some_and(|token| token.text == "(")
            {
                let (tokens, next) = parenthesized(&component.body, index + 1)?;
                expressions.push(tokens);
                index = next;
            } else {
                index += 1;
            }
        }
        if expressions.is_empty() {
            return Err(format!(
                "Component `{}` must render at least one Text expression",
                component.name
            ));
        }
    } else {
        let (nodes, _) = parse_ui_nodes(&component.body, open + 1, true)?;
        if nodes.is_empty() {
            return Err(format!(
                "Static component `{}` must render at least one UI node",
                component.name
            ));
        }
        ui_nodes = nodes;
    }
    Ok(ComponentPlan {
        component: component.name.clone(),
        binding,
        procedure,
        text_expressions: expressions,
        ui_nodes,
    })
}

fn parse_ui_nodes(
    tokens: &[Token],
    mut index: usize,
    stop_at_brace: bool,
) -> Result<(Vec<UiNode>, usize), String> {
    let mut nodes = Vec::new();
    while index < tokens.len() {
        if stop_at_brace && tokens[index].text == "}" {
            return Ok((nodes, index + 1));
        }
        if matches!(tokens[index].text.as_str(), "," | ";") {
            index += 1;
            continue;
        }
        if tokens[index].kind != TokenKind::Identifier {
            return Err(format!(
                "Expected a UI node, found `{}` at {}:{}",
                tokens[index].text, tokens[index].span.line, tokens[index].span.column
            ));
        }
        let name = tokens[index].text.clone();
        index += 1;
        if tokens.get(index).is_none_or(|token| token.text != "(") {
            return Err(format!("UI node `{name}` must use parentheses"));
        }
        index += 1;
        let mut arguments = Vec::new();
        while index < tokens.len() && tokens[index].text != ")" {
            match tokens[index].kind {
                TokenKind::String => arguments.push(UiArgument::String(tokens[index].text.clone())),
                TokenKind::Number => arguments.push(UiArgument::Number(tokens[index].text.clone())),
                _ => {
                    return Err(format!(
                        "UI node `{name}` accepts only literal string and number arguments; found `{}`",
                        tokens[index].text
                    ));
                }
            }
            index += 1;
            if tokens.get(index).is_some_and(|token| token.text == ",") {
                index += 1;
            } else if tokens.get(index).is_none_or(|token| token.text != ")") {
                return Err(format!("Expected `,` or `)` in UI node `{name}`"));
            }
        }
        if tokens.get(index).is_none_or(|token| token.text != ")") {
            return Err(format!("Unterminated UI node `{name}`"));
        }
        index += 1;
        let children = if tokens.get(index).is_some_and(|token| token.text == "{") {
            let (children, next) = parse_ui_nodes(tokens, index + 1, true)?;
            index = next;
            children
        } else {
            Vec::new()
        };
        nodes.push(UiNode {
            name,
            arguments,
            children,
        });
    }
    if stop_at_brace {
        Err("Unterminated UI block".into())
    } else {
        Ok((nodes, index))
    }
}

fn parenthesized(tokens: &[Token], open: usize) -> Result<(Vec<Token>, usize), String> {
    let mut depth = 0usize;
    let mut output = Vec::new();
    for (index, token) in tokens.iter().enumerate().skip(open + 1) {
        match token.text.as_str() {
            "(" => {
                depth += 1;
                output.push(token.clone());
            }
            ")" if depth == 0 => return Ok((output, index + 1)),
            ")" => {
                depth -= 1;
                output.push(token.clone());
            }
            _ => output.push(token.clone()),
        }
    }
    Err("Unterminated parenthesized UI expression".into())
}

fn elm_static_main(plan: &ComponentPlan) -> Result<String, String> {
    let rendered = plan
        .ui_nodes
        .iter()
        .map(elm_ui_node)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n            , ");
    Ok(format!(
        "module Main exposing (main)\n\nimport Browser\nimport Html exposing (Html, a, article, aside, code, div, em, footer, h1, h2, h3, header, hr, img, li, main_, nav, p, pre, section, small, span, strong, text, ul)\nimport Html.Attributes exposing (alt, attribute, class, href, id, rel, src, target)\n\ntype Msg\n    = NoOp\n\nmain : Program () () Msg\nmain =\n    Browser.sandbox\n        {{ init = ()\n        , update = \\_ model -> model\n        , view = view\n        }}\n\nview : () -> Html Msg\nview _ =\n    div [ class \"bind-site\" ]\n        [ {rendered}\n        ]\n"
    ))
}

fn elm_ui_node(node: &UiNode) -> Result<String, String> {
    if node.name == "Text" {
        if !node.children.is_empty() || node.arguments.len() != 1 {
            return Err("`Text` requires exactly one string and no children".into());
        }
        return Ok(format!("text \"{}\"", elm_escape(ui_string(node, 0)?)));
    }

    let children = node
        .children
        .iter()
        .map(elm_ui_node)
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");

    if node.name == "Heading" {
        let level = ui_number(node, 0)?;
        let element = match level {
            "1" => "h1",
            "2" => "h2",
            "3" => "h3",
            _ => return Err("`Heading` level must be 1, 2, or 3".into()),
        };
        let attributes = elm_class_attribute(ui_optional_string(node, 1)?);
        return Ok(format!("{element} [{attributes}] [{children}]"));
    }

    if node.name == "Link" {
        let url = ui_string(node, 0)?;
        let class_name = ui_optional_string(node, 1)?;
        let link_target = ui_optional_string(node, 2)?;
        let label = ui_optional_string(node, 3)?;
        let mut attributes = vec![format!("href \"{}\"", elm_escape(url))];
        if let Some(value) = class_name {
            attributes.push(format!("class \"{}\"", elm_escape(value)));
        }
        if let Some(value) = link_target {
            attributes.push(format!("target \"{}\"", elm_escape(value)));
            if value == "_blank" {
                attributes.push("rel \"noopener noreferrer\"".into());
            }
        }
        if let Some(value) = label {
            attributes.push(format!(
                "attribute \"aria-label\" \"{}\"",
                elm_escape(value)
            ));
        }
        return Ok(format!("a [{}] [{children}]", attributes.join(", ")));
    }

    if node.name == "Image" {
        if !node.children.is_empty() {
            return Err("`Image` cannot have children".into());
        }
        let mut attributes = vec![
            format!("src \"{}\"", elm_escape(ui_string(node, 0)?)),
            format!("alt \"{}\"", elm_escape(ui_string(node, 1)?)),
        ];
        if let Some(value) = ui_optional_string(node, 2)? {
            attributes.push(format!("class \"{}\"", elm_escape(value)));
        }
        return Ok(format!("img [{}] []", attributes.join(", ")));
    }

    if node.name == "Rule" {
        if !node.children.is_empty() {
            return Err("`Rule` cannot have children".into());
        }
        let attributes = elm_class_attribute(ui_optional_string(node, 0)?);
        return Ok(format!("hr [{attributes}] []"));
    }

    let element = match node.name.as_str() {
        "Page" | "Container" | "Div" | "Grid" | "Stack" => "div",
        "Header" => "header",
        "Nav" => "nav",
        "Main" => "main_",
        "Section" => "section",
        "Article" => "article",
        "Aside" => "aside",
        "Footer" => "footer",
        "Paragraph" => "p",
        "Span" => "span",
        "Strong" => "strong",
        "Emphasis" => "em",
        "Small" => "small",
        "List" => "ul",
        "Item" => "li",
        "Pre" => "pre",
        "Code" => "code",
        unsupported => return Err(format!("Unsupported certified UI node `{unsupported}`")),
    };
    if node.arguments.len() > 2 {
        return Err(format!(
            "UI node `{}` accepts at most a class name and an id argument",
            node.name
        ));
    }
    let mut attributes = Vec::new();
    if let Some(value) = ui_optional_string(node, 0)? {
        attributes.push(format!("class \"{}\"", elm_escape(value)));
    }
    if let Some(value) = ui_optional_string(node, 1)? {
        attributes.push(format!("id \"{}\"", elm_escape(value)));
    }
    Ok(format!(
        "{element} [{}] [{children}]",
        attributes.join(", ")
    ))
}

fn ui_string(node: &UiNode, index: usize) -> Result<&str, String> {
    match node.arguments.get(index) {
        Some(UiArgument::String(value)) => Ok(value),
        Some(UiArgument::Number(_)) => Err(format!(
            "Argument {} of `{}` must be a string",
            index + 1,
            node.name
        )),
        None => Err(format!(
            "UI node `{}` is missing argument {}",
            node.name,
            index + 1
        )),
    }
}

fn ui_optional_string(node: &UiNode, index: usize) -> Result<Option<&str>, String> {
    match node.arguments.get(index) {
        Some(UiArgument::String(value)) => Ok(Some(value)),
        Some(UiArgument::Number(_)) => Err(format!(
            "Argument {} of `{}` must be a string",
            index + 1,
            node.name
        )),
        None => Ok(None),
    }
}

fn ui_number(node: &UiNode, index: usize) -> Result<&str, String> {
    match node.arguments.get(index) {
        Some(UiArgument::Number(value)) => Ok(value),
        Some(UiArgument::String(_)) => Err(format!(
            "Argument {} of `{}` must be a number",
            index + 1,
            node.name
        )),
        None => Err(format!(
            "UI node `{}` is missing argument {}",
            node.name,
            index + 1
        )),
    }
}

fn elm_class_attribute(value: Option<&str>) -> String {
    value
        .map(|value| format!("class \"{}\"", elm_escape(value)))
        .unwrap_or_default()
}

fn elm_main(app: &VerifiedApplication, plan: &ComponentPlan) -> Result<String, String> {
    let Some(procedure_name) = plan.procedure.as_deref() else {
        return elm_static_main(plan);
    };
    let binding = plan
        .binding
        .as_deref()
        .ok_or_else(|| "Dynamic component has no bound server result".to_string())?;
    let component = app
        .program
        .functions
        .iter()
        .find(|function| function.name == plan.component)
        .ok_or_else(|| "Missing component".to_string())?;
    let procedure = app
        .program
        .functions
        .iter()
        .find(|function| function.name == procedure_name)
        .ok_or_else(|| format!("Missing server procedure `{procedure_name}`"))?;
    for parameter in &component.parameters {
        if !elm_text_input_type(&parameter.declared_type(), app) {
            return Err(format!(
                "Component input `{}` uses `{}`, which has no lossless text-input decoder yet",
                parameter.name,
                parameter.declared_type().display()
            ));
        }
    }
    let mut output = String::from(
        "module Main exposing (main)\n\nimport Browser\nimport Html exposing (Html, button, div, form, h1, input, label, main_, p, text)\nimport Html.Attributes exposing (class, placeholder, type_, value)\nimport Html.Events exposing (onInput, onSubmit)\nimport Http\nimport Json.Decode as D exposing (Decoder)\nimport Json.Encode as E\n\n",
    );
    for declaration in &app.program.types {
        output.push_str(&elm_type_declaration(declaration, app)?);
    }
    output.push_str("andMap : Decoder a -> Decoder (a -> b) -> Decoder b\nandMap valueDecoder functionDecoder =\n    D.map2 (|>) valueDecoder functionDecoder\n\n");
    output.push_str(INT_CODEC);
    for declaration in &app.program.types {
        output.push_str(&elm_codec(declaration, app)?);
    }

    output.push_str(
        "type RemoteData a\n    = Idle\n    | Loading\n    | Loaded a\n    | Failed String\n\n",
    );
    output.push_str("type alias Model =\n    { state : RemoteData ");
    output.push_str(&elm_type(&procedure.declared_return(), app)?);
    for parameter in &component.parameters {
        output.push_str(&format!(
            "\n    , input{} : String",
            pascal_case(&parameter.name)
        ));
    }
    output.push_str("\n    }\n\n");
    output.push_str("type Msg\n    = Submit\n");
    for parameter in &component.parameters {
        output.push_str(&format!(
            "    | Set{} String\n",
            pascal_case(&parameter.name)
        ));
    }
    output.push_str(&format!(
        "    | GotResponse (Result Http.Error {})\n\n",
        elm_type(&procedure.declared_return(), app)?
    ));
    output.push_str("main : Program () Model Msg\nmain =\n    Browser.element\n        { init = \\_ -> ( initialModel, Cmd.none )\n        , update = update\n        , subscriptions = \\_ -> Sub.none\n        , view = view\n        }\n\ninitialModel : Model\ninitialModel =\n    { state = Idle");
    for parameter in &component.parameters {
        output.push_str(&format!(
            "\n    , input{} = \"\"",
            pascal_case(&parameter.name)
        ));
    }
    output.push_str("\n    }\n\n");
    output.push_str(
        "update : Msg -> Model -> ( Model, Cmd Msg )\nupdate msg model =\n    case msg of\n",
    );
    for parameter in &component.parameters {
        output.push_str(&format!(
            "        Set{} next ->\n            ( {{ model | input{} = next }}, Cmd.none )\n\n",
            pascal_case(&parameter.name),
            pascal_case(&parameter.name)
        ));
    }
    output.push_str("        Submit ->\n            ( { model | state = Loading }\n            , Http.post\n                { url = \"");
    output.push_str(
        &app.rpc_contracts
            .iter()
            .find(|rpc| rpc.name == procedure.name)
            .ok_or_else(|| "Missing RPC contract".to_string())?
            .path,
    );
    output.push_str("\"\n                , body = Http.jsonBody (E.object\n                    [ ");
    for (index, parameter) in component.parameters.iter().enumerate() {
        if index > 0 {
            output.push_str("\n                    , ");
        }
        output.push_str(&format!(
            "( \"{}\", {} )",
            parameter.name,
            elm_encode_text_input(
                &parameter.declared_type(),
                &format!("model.input{}", pascal_case(&parameter.name)),
                app
            )?
        ));
    }
    output.push_str("\n                    ]\n                  )\n                , expect = Http.expectJson GotResponse ");
    output.push_str(&elm_decoder(&procedure.declared_return(), app)?);
    output.push_str("\n                }\n            )\n\n        GotResponse result ->\n            case result of\n                Ok response ->\n                    ( { model | state = Loaded response }, Cmd.none )\n\n                Err error ->\n                    ( { model | state = Failed (httpError error) }, Cmd.none )\n\n");
    output.push_str("view : Model -> Html Msg\nview model =\n    main_ [ class \"shell\" ]\n        [ div [ class \"panel\" ]\n            [ p [ class \"eyebrow\" ] [ text \"BIND CERTIFIED APPLICATION\" ]\n            , h1 [] [ text \"");
    output.push_str(&elm_escape(&plan.component));
    output.push_str("\" ]\n            , form [ onSubmit Submit ]\n                [ ");
    for (index, parameter) in component.parameters.iter().enumerate() {
        if index > 0 {
            output.push_str("\n                , ");
        }
        output.push_str(&format!(
            "label [] [ text \"{}\", input [ type_ \"text\", placeholder \"{}\", value model.input{}, onInput Set{} ] [] ]",
            elm_escape(&humanize(&parameter.name)),
            elm_escape(&parameter.declared_type().display()),
            pascal_case(&parameter.name),
            pascal_case(&parameter.name)
        ));
    }
    if !component.parameters.is_empty() {
        output.push_str("\n                , ");
    }
    output.push_str("button [ type_ \"submit\" ] [ text \"Execute verified call\" ]\n                ]\n            , viewState model.state\n            ]\n        ]\n\n");
    output.push_str(&format!(
        "viewState : RemoteData {} -> Html Msg\nviewState state =\n    case state of\n        Idle ->\n            p [ class \"muted\" ] [ text \"Ready.\" ]\n\n        Loading ->\n            p [ class \"muted\" ] [ text \"Loading…\" ]\n\n        Failed message ->\n            p [ class \"error\" ] [ text message ]\n\n        Loaded value ->\n            div [ class \"result\" ]\n                [ ",
        elm_type(&procedure.declared_return(), app)?
    ));
    for (index, expression) in plan.text_expressions.iter().enumerate() {
        if index > 0 {
            output.push_str("\n                , ");
        }
        output.push_str("p [] [ text ");
        output.push_str(&parenthesize_expr(&elm_ui_text(
            expression,
            binding,
            &procedure.declared_return(),
            app,
        )?));
        output.push_str(" ]");
    }
    output.push_str("\n                ]\n\nhttpError : Http.Error -> String\nhttpError error =\n    case error of\n        Http.BadUrl _ ->\n            \"Invalid server URL\"\n\n        Http.Timeout ->\n            \"The server timed out\"\n\n        Http.NetworkError ->\n            \"The server is unreachable\"\n\n        Http.BadStatus status ->\n            \"Server returned HTTP \" ++ String.fromInt status\n\n        Http.BadBody message ->\n            \"Contract decoding failed: \" ++ message\n");
    Ok(output)
}

/// `Int` travels as a JSON string (`bind-json-v2`). The decoder accepts only the canonical
/// form `-?[0-9]+` (no leading zeros, no `-0`) whose absolute value is at most 2^53 - 1, so an
/// `Int` the Elm runtime cannot hold exactly fails decoding instead of being rounded. It
/// compares digit counts and, for 16 digits, the text against `9007199254740991`. The value is
/// rebuilt from two exact halves because `String.toInt` rounds near 2^53.
const INT_CODEC: &str = r#"intDecoder : D.Decoder Int
intDecoder =
    D.string
        |> D.andThen
            (\text_ ->
                let
                    negative_ =
                        String.startsWith "-" text_

                    digits_ =
                        if negative_ then
                            String.dropLeft 1 text_

                        else
                            text_

                    length_ =
                        String.length digits_

                    canonical_ =
                        length_
                            > 0
                            && String.all Char.isDigit digits_
                            && (digits_ == "0" || not (String.startsWith "0" digits_))
                            && not (negative_ && digits_ == "0")

                    inRange_ =
                        length_ < 16 || (length_ == 16 && digits_ <= "9007199254740991")
                in
                if not canonical_ then
                    D.fail ("Expected an integer string, got \"" ++ text_ ++ "\"")

                else if not inRange_ then
                    D.fail ("Integer out of range (|n| <= 9007199254740991): " ++ text_)

                else
                    -- `String.toInt` accumulates in floating point and rounds near 2^53, so
                    -- the digits are converted in two exact halves.
                    case
                        ( if length_ <= 8 then
                            Just 0

                          else
                            String.toInt (String.dropRight 8 digits_)
                        , String.toInt (String.right 8 digits_)
                        )
                    of
                        ( Just high_, Just low_ ) ->
                            let
                                magnitude_ =
                                    high_ * 100000000 + low_
                            in
                            D.succeed
                                (if negative_ then
                                    negate magnitude_

                                 else
                                    magnitude_
                                )

                        _ ->
                            D.fail ("Expected an integer string, got \"" ++ text_ ++ "\"")
            )

encodeInt : Int -> E.Value
encodeInt value_ =
    E.string (String.fromInt value_)

"#;

fn elm_type_declaration(
    declaration: &TypeDecl,
    app: &VerifiedApplication,
) -> Result<String, String> {
    match declaration.kind {
        TypeKind::Distinct => Ok(format!(
            "type {}\n    = {} {}\n\n",
            declaration.name,
            declaration.name,
            parenthesize_type(&elm_type(
                declaration.underlying.as_ref().ok_or_else(|| format!(
                    "Distinct type `{}` has no underlying type",
                    declaration.name
                ))?,
                app
            )?)
        )),
        TypeKind::Data | TypeKind::Entity => {
            let mut output = format!("type alias {} =\n    {{ ", declaration.name);
            for (index, field) in declaration.fields.iter().enumerate() {
                if index > 0 {
                    output.push_str("\n    , ");
                }
                output.push_str(&format!(
                    "{} : {}",
                    elm_identifier(&field.name),
                    elm_type(&field.type_ref, app)?
                ));
            }
            output.push_str("\n    }\n\n");
            Ok(output)
        }
        TypeKind::Enum | TypeKind::Sealed => Ok(format!(
            "type {}\n    = {}\n\n",
            declaration.name,
            declaration
                .variants
                .iter()
                .map(|variant| pascal_case(variant))
                .collect::<Vec<_>>()
                .join("\n    | ")
        )),
    }
}

fn elm_codec(declaration: &TypeDecl, app: &VerifiedApplication) -> Result<String, String> {
    let lower = lower_camel(&declaration.name);
    match declaration.kind {
        TypeKind::Distinct => {
            let underlying = declaration.underlying.as_ref().ok_or_else(|| {
                format!(
                    "Distinct type `{}` has no underlying type",
                    declaration.name
                )
            })?;
            Ok(format!(
                "decode{} : Decoder {}\ndecode{} =\n    D.map {} {}\n\nencode{} : {} -> E.Value\nencode{} ({} value_) =\n    {}\n\n",
                declaration.name,
                declaration.name,
                declaration.name,
                declaration.name,
                parenthesize_expr(&elm_decoder(underlying, app)?),
                declaration.name,
                declaration.name,
                declaration.name,
                declaration.name,
                elm_encoder(underlying, "value_", app)?
            ))
        }
        TypeKind::Data | TypeKind::Entity => {
            let mut output = format!(
                "decode{} : Decoder {}\ndecode{} =\n    D.succeed {}",
                declaration.name, declaration.name, declaration.name, declaration.name
            );
            for field in &declaration.fields {
                output.push_str(&format!(
                    "\n        |> andMap (D.field \"{}\" {})",
                    field.name,
                    parenthesize_expr(&elm_decoder(&field.type_ref, app)?)
                ));
            }
            output.push_str(&format!(
                "\n\nencode{} : {} -> E.Value\nencode{} value_ =\n    E.object\n        [ ",
                declaration.name, declaration.name, declaration.name
            ));
            for (index, field) in declaration.fields.iter().enumerate() {
                if index > 0 {
                    output.push_str("\n        , ");
                }
                output.push_str(&format!(
                    "( \"{}\", {} )",
                    field.name,
                    elm_encoder(
                        &field.type_ref,
                        &format!("value_.{}", elm_identifier(&field.name)),
                        app
                    )?
                ));
            }
            output.push_str("\n        ]\n\n");
            let _ = lower;
            Ok(output)
        }
        TypeKind::Enum | TypeKind::Sealed => {
            let mut output = format!(
                "decode{} : Decoder {}\ndecode{} =\n    D.string\n        |> D.andThen\n            (\\value_ ->\n                case value_ of\n",
                declaration.name, declaration.name, declaration.name
            );
            for variant in &declaration.variants {
                output.push_str(&format!(
                    "                    \"{}\" ->\n                        D.succeed {}\n\n",
                    lower_camel(variant),
                    pascal_case(variant)
                ));
            }
            output.push_str(&format!(
                "                    _ ->\n                        D.fail \"Unknown {} variant\"\n            )\n\nencode{} : {} -> E.Value\nencode{} value_ =\n    E.string <|\n        case value_ of\n",
                declaration.name, declaration.name, declaration.name, declaration.name
            ));
            for variant in &declaration.variants {
                output.push_str(&format!(
                    "            {} ->\n                \"{}\"\n\n",
                    pascal_case(variant),
                    lower_camel(variant)
                ));
            }
            Ok(output)
        }
    }
}

fn elm_type(type_ref: &TypeRef, app: &VerifiedApplication) -> Result<String, String> {
    let parsed = ParsedType::parse(&type_ref.name)?;
    let mut output = match parsed.base.as_str() {
        "Unit" => "()".to_string(),
        "String" | "Decimal" | "Money" | "Email" | "Url" | "Date" | "DateTime" | "Instant"
        | "Duration" | "UUID" => "String".to_string(),
        "Int" | "Long" => "Int".to_string(),
        "Float" | "Double" => "Float".to_string(),
        "Boolean" => "Bool".to_string(),
        "List" if parsed.arguments.len() == 1 => format!(
            "List {}",
            parenthesize_type(&elm_type(&TypeRef::new(parsed.arguments[0].render()), app)?)
        ),
        custom if app.program.types.iter().any(|item| item.name == custom) => custom.to_string(),
        unsupported => return Err(format!("Unsupported Elm type `{unsupported}`")),
    };
    if type_ref.nullable {
        output = format!("Maybe {}", parenthesize_type(&output));
    }
    Ok(output)
}

fn elm_decoder(type_ref: &TypeRef, app: &VerifiedApplication) -> Result<String, String> {
    let parsed = ParsedType::parse(&type_ref.name)?;
    let base = match parsed.base.as_str() {
        "Unit" => "D.succeed ()".to_string(),
        "String" | "Decimal" | "Money" | "Email" | "Url" | "Date" | "DateTime" | "Instant"
        | "Duration" | "UUID" => "D.string".to_string(),
        "Int" => "intDecoder".to_string(),
        "Long" => "D.int".to_string(),
        "Float" | "Double" => "D.float".to_string(),
        "Boolean" => "D.bool".to_string(),
        "List" if parsed.arguments.len() == 1 => format!(
            "D.list {}",
            parenthesize_expr(&elm_decoder(
                &TypeRef::new(parsed.arguments[0].render()),
                app
            )?)
        ),
        custom if app.program.types.iter().any(|item| item.name == custom) => {
            format!("decode{custom}")
        }
        unsupported => return Err(format!("Unsupported Elm decoder `{unsupported}`")),
    };
    if type_ref.nullable {
        Ok(format!("D.nullable {}", parenthesize_expr(&base)))
    } else {
        Ok(base)
    }
}

fn elm_encoder(
    type_ref: &TypeRef,
    value: &str,
    app: &VerifiedApplication,
) -> Result<String, String> {
    if type_ref.nullable {
        let inner = TypeRef {
            name: type_ref.name.clone(),
            nullable: false,
        };
        return Ok(format!(
            "Maybe.withDefault E.null (Maybe.map (\\item_ -> {}) {value})",
            elm_encoder(&inner, "item_", app)?
        ));
    }
    let parsed = ParsedType::parse(&type_ref.name)?;
    match parsed.base.as_str() {
        "Unit" => Ok("E.null".to_string()),
        "String" | "Decimal" | "Money" | "Email" | "Url" | "Date" | "DateTime" | "Instant"
        | "Duration" | "UUID" => Ok(format!("E.string {value}")),
        "Int" => Ok(format!("encodeInt {value}")),
        "Long" => Ok(format!("E.int {value}")),
        "Float" | "Double" => Ok(format!("E.float {value}")),
        "Boolean" => Ok(format!("E.bool {value}")),
        "List" if parsed.arguments.len() == 1 => Ok(format!(
            "E.list (\\item_ -> {}) {value}",
            elm_encoder(&TypeRef::new(parsed.arguments[0].render()), "item_", app)?
        )),
        custom if app.program.types.iter().any(|item| item.name == custom) => {
            Ok(format!("encode{custom} {value}"))
        }
        unsupported => Err(format!("Unsupported Elm encoder `{unsupported}`")),
    }
}

fn elm_ui_text(
    tokens: &[Token],
    binding: &str,
    response: &TypeRef,
    app: &VerifiedApplication,
) -> Result<String, String> {
    if tokens.len() == 1 && tokens[0].kind == TokenKind::String {
        return Ok(format!("\"{}\"", elm_escape(&tokens[0].text)));
    }
    if tokens.len() == 3 && tokens[0].text == binding && tokens[1].text == "." {
        let declaration = app
            .program
            .types
            .iter()
            .find(|declaration| declaration.name == response.name)
            .ok_or_else(|| format!("Response `{}` is not a record", response.name))?;
        let field = declaration
            .fields
            .iter()
            .find(|field| field.name == tokens[2].text)
            .ok_or_else(|| format!("Unknown UI field `{}.{}`", binding, tokens[2].text))?;
        return elm_to_string(
            &field.type_ref,
            &format!("value.{}", elm_identifier(&field.name)),
            app,
        );
    }
    Err(format!(
        "UI Text expression `{}` is outside the certified subset",
        tokens
            .iter()
            .map(|token| token.text.as_str())
            .collect::<String>()
    ))
}

fn elm_to_string(
    type_ref: &TypeRef,
    value: &str,
    app: &VerifiedApplication,
) -> Result<String, String> {
    if type_ref.nullable {
        let inner = TypeRef {
            name: type_ref.name.clone(),
            nullable: false,
        };
        return Ok(format!(
            "Maybe.withDefault \"—\" (Maybe.map (\\item_ -> {}) {value})",
            elm_to_string(&inner, "item_", app)?
        ));
    }
    if let Some(declaration) = distinct_declaration(&type_ref.name, app) {
        let underlying = declaration.underlying.as_ref().ok_or_else(|| {
            format!(
                "Distinct type `{}` has no underlying type",
                declaration.name
            )
        })?;
        return Ok(format!(
            "(case {value} of\n                    {} inner_ ->\n                        {}\n                )",
            declaration.name,
            elm_to_string(underlying, "inner_", app)?
        ));
    }
    match type_ref.name.as_str() {
        "String" | "Decimal" | "Money" | "Email" | "Url" | "Date" | "DateTime" | "Instant"
        | "Duration" | "UUID" => Ok(value.to_string()),
        "Int" | "Long" => Ok(format!("String.fromInt {value}")),
        "Float" | "Double" => Ok(format!("String.fromFloat {value}")),
        "Boolean" => Ok(format!("if {value} then \"true\" else \"false\"")),
        other => Err(format!("Cannot render `{other}` as certified Text")),
    }
}

fn elm_text_input_type(type_ref: &TypeRef, app: &VerifiedApplication) -> bool {
    if type_ref.nullable {
        return false;
    }
    let base = underlying_name(&type_ref.name, app);
    matches!(
        base.as_str(),
        "String"
            | "UUID"
            | "Email"
            | "Url"
            | "Decimal"
            | "Money"
            | "Date"
            | "DateTime"
            | "Instant"
            | "Duration"
            | "Int"
    )
}

fn elm_encode_text_input(
    type_ref: &TypeRef,
    value: &str,
    app: &VerifiedApplication,
) -> Result<String, String> {
    if let Some(declaration) = distinct_declaration(&type_ref.name, app) {
        let underlying = declaration.underlying.as_ref().ok_or_else(|| {
            format!(
                "Distinct type `{}` has no underlying type",
                declaration.name
            )
        })?;
        return elm_encode_text_input(underlying, value, app);
    }
    match underlying_name(&type_ref.name, app).as_str() {
        "String" | "UUID" | "Email" | "Url" | "Decimal" | "Money" | "Date" | "DateTime"
        | "Instant" | "Duration" => Ok(format!("E.string {value}")),
        // The server validates the canonical form; the text is sent as the user typed it.
        "Int" => Ok(format!("E.string (String.trim {value})")),
        other => Err(format!("No certified text input encoder for `{other}`")),
    }
}

/// The procedures of the `bind-json-v2` contract: for each input field and the output, the
/// Bind type and its JSON encoding (`Int`, `Decimal` and `Money` are strings).
fn contract_schema(app: &VerifiedApplication) -> String {
    let describe = |type_ref: &TypeRef| {
        format!(
            "{{\"type\":{},\"json\":{}}}",
            json(&type_ref.display()),
            json(&wire_json(&parse_wire_type(type_ref), app, 0))
        )
    };
    let procedures = app
        .rpc_contracts
        .iter()
        .map(|rpc| {
            format!(
                "    {}: {{\"path\":{},\"input\":{{{}}},\"output\":{}}}",
                json(&rpc.name),
                json(&rpc.path),
                rpc.parameters
                    .iter()
                    .map(|parameter| format!(
                        "{}:{}",
                        json(&parameter.name),
                        describe(&parameter.declared_type())
                    ))
                    .collect::<Vec<_>>()
                    .join(","),
                describe(&rpc.return_type)
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "{{\n  \"wireVersion\": \"{WIRE_VERSION}\",\n  \"procedures\": {{\n{procedures}\n  }}\n}}\n"
    )
}

/// For each procedure, a valid input and malformed ones: each field in turn replaced by a
/// value of the wrong shape (for example `"1.5"` for an `Int`), and an unknown field.
fn contract_vectors(app: &VerifiedApplication) -> String {
    let vectors = app
        .rpc_contracts
        .iter()
        .map(|rpc| {
            let types: Vec<TypeExpr> = rpc
                .parameters
                .iter()
                .map(|parameter| parse_wire_type(&parameter.declared_type()))
                .collect();
            let object = |replaced: Option<(usize, &str)>, extra: bool| {
                let mut fields: Vec<String> = rpc
                    .parameters
                    .iter()
                    .zip(&types)
                    .enumerate()
                    .map(|(index, (parameter, ty))| {
                        let value = match replaced {
                            Some((target, value)) if target == index => value.to_string(),
                            _ => sample_json(ty, app, 0),
                        };
                        format!("{}:{value}", json(&parameter.name))
                    })
                    .collect();
                if extra {
                    fields.push("\"bindUnknownField\":0".to_string());
                }
                format!("{{{}}}", fields.join(","))
            };
            let mut invalid = Vec::new();
            for (index, ty) in types.iter().enumerate() {
                if let Some(value) = malformed_json(ty, app) {
                    invalid.push(object(Some((index, &value)), false));
                }
            }
            invalid.push(object(None, true));
            format!(
                "    {{\"procedure\":{},\"validInput\":{},\"invalidInputs\":[{}]}}",
                json(&rpc.name),
                object(None, false),
                invalid.join(",")
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n  \"wireVersion\": \"{WIRE_VERSION}\",\n  \"vectors\": [\n{vectors}\n  ]\n}}\n")
}

/// The structure of a declared wire type (an unparsable one, never produced by a verified
/// program, reads as `Unit`).
fn parse_wire_type(type_ref: &TypeRef) -> TypeExpr {
    parse_type_text(&type_ref.display(), &Span::default()).unwrap_or(TypeExpr::Named {
        name: "Unit".to_string(),
        args: Vec::new(),
        span: Span::default(),
    })
}

/// Nesting beyond which recursive types stop being expanded in samples.
const SAMPLE_DEPTH: usize = 8;

/// The JSON encoding of a wire type: `string`, `number`, `boolean`, `null`, `array` or
/// `object`, with `|null` when optional.
fn wire_json(ty: &TypeExpr, app: &VerifiedApplication, depth: usize) -> String {
    match ty {
        TypeExpr::Nullable(inner) => format!("{}|null", wire_json(inner, app, depth)),
        TypeExpr::Function { .. } => "unsupported".to_string(),
        TypeExpr::Named { name, .. } => match name.as_str() {
            "Unit" => "null".to_string(),
            "Float" | "Double" => "number".to_string(),
            "Boolean" => "boolean".to_string(),
            "List" => "array".to_string(),
            _ => match declaration(name, app) {
                Some(declaration) => match declaration.kind {
                    TypeKind::Data | TypeKind::Entity => "object".to_string(),
                    TypeKind::Enum | TypeKind::Sealed => "string".to_string(),
                    TypeKind::Distinct => match &declaration.underlying {
                        Some(underlying) if depth < SAMPLE_DEPTH => {
                            wire_json(&parse_wire_type(underlying), app, depth + 1)
                        }
                        _ => "unsupported".to_string(),
                    },
                },
                None => "string".to_string(),
            },
        },
    }
}

/// A valid JSON value of a wire type.
fn sample_json(ty: &TypeExpr, app: &VerifiedApplication, depth: usize) -> String {
    let TypeExpr::Named { name, .. } = ty else {
        return "null".to_string();
    };
    match name.as_str() {
        "Unit" => "null".to_string(),
        "UUID" => json("00000000-0000-4000-8000-000000000000"),
        "String" | "Email" | "Url" => json("sample"),
        "Int" | "Decimal" | "Money" => json("0"),
        "Date" => json("2026-01-01"),
        "DateTime" => json("2026-01-01T00:00:00"),
        "Instant" => json("2026-01-01T00:00:00Z"),
        "Float" | "Double" => "0".to_string(),
        "Boolean" => "false".to_string(),
        "List" => "[]".to_string(),
        _ if depth >= SAMPLE_DEPTH => "null".to_string(),
        _ => match declaration(name, app) {
            Some(declaration) => match declaration.kind {
                TypeKind::Data | TypeKind::Entity => format!(
                    "{{{}}}",
                    declaration
                        .fields
                        .iter()
                        .map(|field| format!(
                            "{}:{}",
                            json(&field.name),
                            sample_json(&parse_wire_type(&field.type_ref), app, depth + 1)
                        ))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                TypeKind::Enum | TypeKind::Sealed => declaration
                    .variants
                    .first()
                    .map_or_else(|| "null".to_string(), |variant| json(&lower_camel(variant))),
                TypeKind::Distinct => declaration.underlying.as_ref().map_or_else(
                    || "null".to_string(),
                    |underlying| sample_json(&parse_wire_type(underlying), app, depth + 1),
                ),
            },
            None => "null".to_string(),
        },
    }
}

/// A JSON value that a wire type must reject, if the type has one.
fn malformed_json(ty: &TypeExpr, app: &VerifiedApplication) -> Option<String> {
    let TypeExpr::Named { name, .. } = ty else {
        // Optional: a value of the wrong shape for the inner type.
        return match ty {
            TypeExpr::Nullable(inner) => malformed_json(inner, app),
            _ => None,
        };
    };
    Some(match name.as_str() {
        "Unit" => return None,
        "Int" => json("1.5"),
        "Decimal" | "Money" => json("1.5.0"),
        "UUID" => json("not-a-uuid"),
        "Date" => json("2026-13-01"),
        "DateTime" | "Instant" => json("2026-01-01 25:00"),
        "String" | "Email" | "Url" => "0".to_string(),
        "Float" | "Double" => json("0"),
        "Boolean" => json("false"),
        "List" => "{}".to_string(),
        _ => match declaration(name, app)?.kind {
            TypeKind::Data | TypeKind::Entity => "[]".to_string(),
            TypeKind::Enum | TypeKind::Sealed => json("bindUnknownVariant"),
            TypeKind::Distinct => {
                let underlying = declaration(name, app)?.underlying.as_ref()?;
                return malformed_json(&parse_wire_type(underlying), app);
            }
        },
    })
}

fn declaration<'a>(name: &str, app: &'a VerifiedApplication) -> Option<&'a TypeDecl> {
    app.program
        .types
        .iter()
        .find(|declaration| declaration.name == name)
}

fn generated_manifest(app: &VerifiedApplication, target: &TargetConfig) -> String {
    format!(
        "{{\n  \"languageVersion\": \"0.1.0\",\n  \"compilerVersion\": \"bindc-0.1.0\",\n  \"profile\": \"{PROFILE_VERSION}\",\n  \"certified\": false,\n  \"certificationState\": \"generated\",\n  \"project\": {},\n  \"target\": {},\n  \"deployment\": {},\n  \"sourceHash\": {},\n  \"schemaFingerprint\": {},\n  \"wireVersion\": \"{WIRE_VERSION}\",\n  \"requiredCapabilities\": {}\n}}\n",
        json(&app.project_name),
        json(&app.target),
        json(&target.deployment),
        json(&app.source_hash),
        json(&app.database.fingerprint),
        json_array(
            &app.required_capabilities
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        )
    )
}

fn aws_template(app: &VerifiedApplication, database_environment: &str) -> String {
    format!(
        "AWSTemplateFormatVersion: '2010-09-09'\nTransform: AWS::Serverless-2016-10-31\nDescription: Bind certified deployment for {}\nResources:\n  BindApi:\n    Type: AWS::Serverless::Function\n    Metadata:\n      BuildMethod: rust-cargolambda\n      BuildProperties:\n        Binary: bootstrap\n    Properties:\n      Runtime: provided.al2023\n      Architectures: [arm64]\n      Handler: bootstrap\n      CodeUri: ./\n      Events:\n        Api:\n          Type: HttpApi\n          Properties:\n            Path: /api/{{proxy+}}\n            Method: ANY\n      Environment:\n        Variables:\n          {}: !Ref DatabaseUrl\nParameters:\n  DatabaseUrl:\n    Type: String\n    NoEcho: true\n",
        app.project_name, database_environment
    )
}

/// A Vercel function: a hand-built tokio runtime with 64 MiB worker stacks, as in
/// `AWS_MAIN`, serving the procedure at `path`.
fn vercel_main(path: &str, database_environment: &str) -> String {
    format!(
        "// GENERATED BY BINDC. DO NOT EDIT.\n#![forbid(unsafe_code)]\n#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable)]\n\nuse http_body_util::BodyExt;\nuse sqlx::{{PgPool, postgres::PgPoolOptions}};\nuse vercel_runtime::{{Error, Request, Response, run, service_fn}};\n\nfn main() -> Result<(), Error> {{\n    let runtime = tokio::runtime::Builder::new_multi_thread()\n        .enable_all()\n        .thread_stack_size(64 * 1024 * 1024)\n        .build()?;\n    runtime.block_on(serve())\n}}\n\nasync fn serve() -> Result<(), Error> {{\n    let database_url = std::env::var({})?;\n    let pool = PgPoolOptions::new().max_connections(5).connect_lazy(&database_url)?;\n    run(service_fn(move |request| {{\n        let pool = pool.clone();\n        async move {{ handler(request, pool).await }}\n    }})).await\n}}\n\nasync fn handler(request: Request, pool: PgPool) -> Result<Response<String>, Error> {{\n    let bytes = request.into_body().collect().await?.to_bytes();\n    let result = {}::serve_rpc({}.to_string(), bytes.to_vec(), pool).await;\n    Ok(Response::builder()\n        .status(result.status)\n        .header(\"content-type\", \"application/json; charset=utf-8\")\n        .body(result.body)?)\n}}\n",
        rust_string(database_environment),
        package_name_for_code(),
        rust_string(path)
    )
}

fn generated_readme(target: &TargetConfig) -> String {
    let deployment = if target.deployment == "aws-lambda" {
        "Run `sam build --beta-features`, then `sam deploy --guided`. The generated SAM metadata delegates the Linux arm64 build to Cargo Lambda."
    } else {
        "Run `vercel deploy`. Vercel builds every Rust handler declared in Cargo.toml and serves the Elm output from `web/dist`."
    };
    format!(
        "# Generated Bind application\n\nProfile: `{PROFILE_VERSION}`  \nDeployment: `{}`\n\nThis directory is generated. `bind build` compiles Rust and Elm; `bind certify` also runs contract tests and writes an artifact attestation.\n\n{deployment}\n",
        target.deployment
    )
}

fn package_name(name: &str) -> String {
    let _ = name;
    "bind-app".to_string()
}

// Generated binaries refer to the Cargo package library; Cargo normalizes '-' to '_'.
fn package_name_for_code() -> &'static str {
    "bind_app"
}

fn distinct_declaration<'a>(name: &str, app: &'a VerifiedApplication) -> Option<&'a TypeDecl> {
    app.program
        .types
        .iter()
        .find(|declaration| declaration.kind == TypeKind::Distinct && declaration.name == name)
}

fn underlying_name(name: &str, app: &VerifiedApplication) -> String {
    let mut current = name.to_string();
    let mut remaining = app.program.types.len() + 1;
    while remaining > 0 {
        remaining -= 1;
        let Some(declaration) = distinct_declaration(&current, app) else {
            break;
        };
        let Some(underlying) = &declaration.underlying else {
            break;
        };
        current = underlying.name.clone();
    }
    current
}

fn rust_string(value: &str) -> String {
    format!("{:?}", value)
}

fn elm_identifier(value: &str) -> String {
    match value {
        "type" | "module" | "port" | "where" | "let" | "in" | "case" | "of" | "if" | "then"
        | "else" => format!("{value}_"),
        _ => lower_camel(value),
    }
}

fn snake_case(value: &str) -> String {
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                output.push('_');
            }
            output.push(ch.to_ascii_lowercase());
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            output.push(ch.to_ascii_lowercase());
        } else {
            output.push('_');
        }
    }
    output
}

fn pascal_case(value: &str) -> String {
    let mut output = String::new();
    let mut upper = true;
    for ch in value.chars() {
        if !ch.is_ascii_alphanumeric() {
            upper = true;
        } else if upper {
            output.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            output.push(ch);
        }
    }
    output
}

pub(crate) fn lower_camel(value: &str) -> String {
    let pascal = pascal_case(value);
    let mut chars = pascal.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_lowercase(), chars.as_str()),
        None => String::new(),
    }
}

fn humanize(value: &str) -> String {
    let snake = snake_case(value);
    let mut words = snake.split('_');
    match words.next() {
        Some(first) => format!(
            "{}{}",
            pascal_case(first),
            words.map(|word| format!(" {word}")).collect::<String>()
        ),
        None => value.to_string(),
    }
}

fn elm_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn parenthesize_type(value: &str) -> String {
    if value.contains(' ') {
        format!("({value})")
    } else {
        value.to_string()
    }
}

fn parenthesize_expr(value: &str) -> String {
    if value.contains(' ') {
        format!("({value})")
    } else {
        value.to_string()
    }
}

#[derive(Clone, Debug)]
struct ParsedType {
    base: String,
    arguments: Vec<ParsedType>,
}

impl ParsedType {
    fn parse(value: &str) -> Result<Self, String> {
        let chars: Vec<char> = value.chars().collect();
        let mut index = 0usize;
        let parsed = Self::parse_at(&chars, &mut index)?;
        if index != chars.len() {
            return Err(format!("Invalid type `{value}`"));
        }
        Ok(parsed)
    }

    fn parse_at(chars: &[char], index: &mut usize) -> Result<Self, String> {
        let start = *index;
        while chars
            .get(*index)
            .is_some_and(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '.')
        {
            *index += 1;
        }
        if start == *index {
            return Err("Expected type name".into());
        }
        let base = chars[start..*index].iter().collect();
        let mut arguments = Vec::new();
        if chars.get(*index) == Some(&'<') {
            *index += 1;
            loop {
                arguments.push(Self::parse_at(chars, index)?);
                match chars.get(*index) {
                    Some(',') => *index += 1,
                    Some('>') => {
                        *index += 1;
                        break;
                    }
                    _ => return Err("Unterminated generic type".into()),
                }
            }
        }
        Ok(Self { base, arguments })
    }

    fn render(&self) -> String {
        if self.arguments.is_empty() {
            self.base.clone()
        } else {
            format!(
                "{}<{}>",
                self.base,
                self.arguments
                    .iter()
                    .map(Self::render)
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

/// `src/lib.rs` of an application with a static client and no endpoints.
const STATIC_LIBRARY: &str = "// GENERATED BY BINDC. DO NOT EDIT.\n#![forbid(unsafe_code)]\n#![deny(unused_must_use)]\n\n/// Marker for a Bind application with a static client and no server boundary.\npub const STATIC_CLIENT: bool = true;\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn static_client_has_no_runtime_server() {\n        assert!(std::hint::black_box(super::STATIC_CLIENT));\n    }\n}\n";

fn aws_main(database_environment: &str) -> String {
    AWS_MAIN.replace("__BIND_DATABASE_ENV__", database_environment)
}

/// The Lambda entry point. The tokio runtime is built by hand: its worker threads get
/// 64 MiB stacks, and every request runs on one of them (`serve_rpc` spawns it), so deep Bind
/// recursion reaches `RecursionLimit` instead of overflowing a stack.
const AWS_MAIN: &str = r#"// GENERATED BY BINDC. DO NOT EDIT.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable)]

use lambda_http::{Body, Error, Request, Response, run, service_fn};
use sqlx::postgres::PgPoolOptions;

fn main() -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(64 * 1024 * 1024)
        .build()?;
    runtime.block_on(serve())
}

async fn serve() -> Result<(), Error> {
    let database_url = std::env::var("__BIND_DATABASE_ENV__")?;
    let pool = PgPoolOptions::new().max_connections(5).connect_lazy(&database_url)?;
    run(service_fn(move |request: Request| {
        let pool = pool.clone();
        async move {
            let path = request.uri().path().to_string();
            let body = match request.body() {
                Body::Empty => Vec::new(),
                Body::Text(value) => value.as_bytes().to_vec(),
                Body::Binary(value) => value.clone(),
                _ => Vec::new(),
            };
            let result = bind_app::serve_rpc(path, body, pool).await;
            Response::builder()
                .status(result.status)
                .header("content-type", "application/json; charset=utf-8")
                .body(result.body)
                .map_err(Error::from)
        }
    }))
    .await
}
"#;

const ELM_JSON: &str = r#"{
  "type": "application",
  "source-directories": ["src"],
  "elm-version": "0.19.2",
  "dependencies": {
    "direct": {
      "elm/browser": "1.0.2",
      "elm/core": "1.0.5",
      "elm/html": "1.0.0",
      "elm/http": "2.0.0",
      "elm/json": "1.1.3"
    },
    "indirect": {
      "elm/bytes": "1.0.8",
      "elm/file": "1.0.5",
      "elm/time": "1.0.0",
      "elm/url": "1.0.0",
      "elm/virtual-dom": "1.0.3"
    }
  },
  "test-dependencies": { "direct": {}, "indirect": {} }
}
"#;

fn index_html(config: &ProjectConfig) -> String {
    format!(
        r##"<!doctype html>
<html lang="{}">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width,initial-scale=1">
    <meta name="description" content="{}">
    <meta name="theme-color" content="#efeee8">
    <meta property="og:title" content="{}">
    <meta property="og:description" content="{}">
    <meta property="og:type" content="website">
    <title>{}</title>
    <link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 64 64'%3E%3Crect width='64' height='64' fill='%2311130f'/%3E%3Ctext x='32' y='45' text-anchor='middle' font-family='monospace' font-size='40' font-weight='700' fill='%23c8ff36'%3EB%3C/text%3E%3C/svg%3E">
    <link rel="stylesheet" href="/styles.css">
  </head>
  <body>
    <div id="app"></div>
    <script src="/main.js"></script>
    <script>Elm.Main.init({{ node: document.getElementById('app') }});</script>
  </body>
</html>
"##,
        html_escape(&config.web_language),
        html_escape(&config.web_description),
        html_escape(&config.web_title),
        html_escape(&config.web_description),
        html_escape(&config.web_title),
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const WEB_CSS: &str = r#":root { color-scheme: dark; font-family: Inter, ui-sans-serif, system-ui, sans-serif; background: #0b0d10; color: #f7f7f4; }
* { box-sizing: border-box; }
body { margin: 0; min-height: 100vh; background: radial-gradient(circle at 20% 0%, #263a33 0, transparent 34rem), #0b0d10; }
.shell { min-height: 100vh; display: grid; place-items: center; padding: 2rem; }
.panel { width: min(42rem, 100%); border-top: 1px solid #8effc1; padding-top: 2rem; }
.eyebrow { color: #8effc1; font: 600 .72rem/1.2 ui-monospace, monospace; letter-spacing: .14em; }
h1 { font-size: clamp(2.4rem, 8vw, 5.6rem); line-height: .9; margin: 1rem 0 2.5rem; letter-spacing: -.06em; }
form { display: grid; gap: 1rem; }
label { display: grid; gap: .45rem; color: #c8ccc9; }
input { width: 100%; border: 1px solid #414843; background: #121613; color: inherit; padding: .9rem 1rem; font: inherit; }
button { border: 0; background: #8effc1; color: #07110c; padding: .95rem 1.2rem; font-weight: 750; cursor: pointer; }
.muted { color: #8a928d; }.error { color: #ff928e; }.result { margin-top: 2rem; border-left: 2px solid #8effc1; padding-left: 1rem; }
"#;

const VERCEL_JSON: &str = r#"{
  "$schema": "https://openapi.vercel.sh/vercel.json",
  "buildCommand": "npm run build:web",
  "outputDirectory": "web/dist"
}
"#;

const VERCEL_PACKAGE_JSON: &str = r#"{
  "name": "bind-generated-web",
  "private": true,
  "scripts": {
    "build:web": "cd web && ../node_modules/.bin/elm make src/Main.elm --optimize --output dist/main.js && cp index.html styles.css dist/"
  },
  "devDependencies": {
    "elm": "0.19.2-0"
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    /// The server dependencies are the runtime's, at the same pinned versions.
    #[test]
    fn server_dependencies_match_the_runtime() {
        let manifest = include_str!("../runtime/bind-runtime/Cargo.toml");
        let runtime = manifest.split_once("[dependencies]").unwrap().1;
        let generated = server_dependencies();
        for line in runtime
            .lines()
            .filter(|line| line.contains("version = ") || line.contains(" = \"="))
        {
            let name = line.split_whitespace().next().unwrap();
            let version = line.split('"').find(|part| part.starts_with('=')).unwrap();
            let generated_line = generated
                .lines()
                .find(|candidate| candidate.split_whitespace().next() == Some(name))
                .unwrap_or_else(|| panic!("`{name}` is missing from the generated dependencies"));
            assert!(
                generated_line.contains(&format!("\"{version}\"")),
                "{name}: {generated_line} vs {line}"
            );
        }
        assert!(generated.contains("features = [\"derive\", \"rc\"]"));
    }

    /// Removes its directory on drop, so a failing test leaves nothing behind.
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The `Main.elm` generated for `tests/codegen/client_int.bd`.
    fn client_int_elm() -> String {
        let cases = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/codegen");
        let guard = TempDir(std::env::temp_dir().join(format!(
            "bindc-client-int-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )));
        let root = &guard.0;
        fs::create_dir_all(root.join("app")).unwrap();
        fs::copy(cases.join("client_int.bd"), root.join("app/main.bd")).unwrap();
        fs::copy(cases.join("client_int.csv"), root.join("schema.csv")).unwrap();
        fs::write(
            root.join("bind.toml"),
            "[project]\nname = \"test\"\nsource = \"app\"\n[database.main]\nschema = \"schema.csv\"\nverification = \"static\"\n[target.vercel]\nclient = \"elm\"\nserver = \"rust\"\ndeployment = \"vercel\"\ncapabilities = [\"ui.dom\", \"server.http\", \"db.read\"]\n",
        )
        .unwrap();
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let result = crate::compiler::verify(&config, "vercel");
        let app = result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        let plan = component_plan(&app).unwrap();
        elm_main(&app, &plan).unwrap()
    }

    /// The Elm client of `tests/codegen/client_int.bd` (`BIND_BLESS=1` regenerates the
    /// snapshot): `Int` travels as a string, through `intDecoder` and `encodeInt`.
    #[test]
    fn client_int_snapshot() {
        let elm = client_int_elm();
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/codegen/client_int.elm.expected");
        if std::env::var_os("BIND_BLESS").is_some() {
            fs::write(&path, &elm).unwrap();
        }
        assert_eq!(fs::read_to_string(&path).unwrap_or_default(), elm);
        for expected in [
            "intDecoder : D.Decoder Int",
            "encodeInt : Int -> E.Value",
            "|> andMap (D.field \"quantity\" intDecoder)",
            "|> andMap (D.field \"reserved\" (D.nullable intDecoder))",
            "|> andMap (D.field \"history\" (D.list intDecoder))",
            "|> andMap (D.field \"stock\" decodeCount)",
            "|> andMap (D.field \"series\" decodeCounts)",
            "|> andMap (D.field \"spare\" decodeMaybeCount)",
            "= Counts (List Int)",
            "= MaybeCount (Maybe Int)",
            "D.map Count intDecoder",
            "D.map Counts (D.list intDecoder)",
            "D.map MaybeCount (D.nullable intDecoder)",
            "( \"stock\", encodeCount value_.stock )",
            "( \"quantity\", encodeInt value_.quantity )",
            "( \"history\", E.list (\\item_ -> encodeInt item_) value_.history )",
            "( \"quantidade\", E.string (String.trim model.inputQuantidade) )",
        ] {
            assert!(elm.contains(expected), "missing `{expected}`");
        }
        // `Long` cannot cross the wire in this profile, so no plain number codec is generated.
        assert!(!elm.contains("D.int") && !elm.contains("E.int"));
    }

    /// Review Focus 1, run for real: the generated `intDecoder` and `encodeInt`, compiled by
    /// `elm` and run by `node`, accept canonical safe integers exactly and reject anything else.
    #[test]
    #[ignore = "needs elm and node; run with `cargo test client_int_decoder -- --ignored`"]
    fn client_int_decoder_runs_in_elm() {
        let available = |tool: &str| {
            std::process::Command::new(tool)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
        };
        if !available("elm") || !available("node") {
            eprintln!("skipped: `elm` and `node` are required");
            return;
        }
        let elm = client_int_elm();
        let start = elm.find("intDecoder : D.Decoder Int").unwrap();
        let end = elm.find("encodeInt value_ =").unwrap()
            + "encodeInt value_ =\n    E.string (String.fromInt value_)\n\n".len();
        let guard =
            TempDir(std::env::temp_dir().join(format!("bindc-int-harness-{}", std::process::id())));
        let dir = &guard.0;
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("elm.json"),
            "{\"type\":\"application\",\"source-directories\":[\"src\"],\"elm-version\":\"0.19.2\",\"dependencies\":{\"direct\":{\"elm/browser\":\"1.0.2\",\"elm/core\":\"1.0.5\",\"elm/html\":\"1.0.0\",\"elm/http\":\"2.0.0\",\"elm/json\":\"1.1.3\"},\"indirect\":{\"elm/bytes\":\"1.0.8\",\"elm/file\":\"1.0.5\",\"elm/time\":\"1.0.0\",\"elm/url\":\"1.0.0\",\"elm/virtual-dom\":\"1.0.3\"}},\"test-dependencies\":{\"direct\":{},\"indirect\":{}}}",
        )
        .unwrap();
        fs::write(
            dir.join("src/Harness.elm"),
            format!(
                "port module Harness exposing (main)\n\nimport Json.Decode as D\nimport Json.Encode as E\n\nport input : (E.Value -> msg) -> Sub msg\n\nport output : String -> Cmd msg\n\n{}run : E.Value -> String\nrun value =\n    case D.decodeValue intDecoder value of\n        Ok n ->\n            \"OK \" ++ String.fromInt n ++ \" \" ++ E.encode 0 (encodeInt n)\n\n        Err _ ->\n            \"FAIL\"\n\nmain : Program () () E.Value\nmain =\n    Platform.worker\n        {{ init = \\_ -> ( (), Cmd.none )\n        , update = \\value _ -> ( (), output (run value) )\n        , subscriptions = \\_ -> input identity\n        }}\n",
                &elm[start..end]
            ),
        )
        .unwrap();
        let compiled = std::process::Command::new("elm")
            .args(["make", "src/Harness.elm", "--output=harness.js"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        fs::write(
            dir.join("run.js"),
            r#"const { Elm } = require('./harness.js');
const app = Elm.Harness.init();
const inputs = [
  "0", "-1", "9007199254740991", "-9007199254740991", "9007199254740990",
  "1000000000000000", "8999999999999999",
  "9007199254740992", "-9007199254740992", "10000000000000000",
  "01", "-0", "+1", "1.5", "", " 1", 1, null,
];
const results = [];
app.ports.output.subscribe((r) => {
  results.push(r);
  if (results.length === inputs.length) console.log(results.join("\n"));
});
for (const input of inputs) app.ports.input.send(input);
"#,
        )
        .unwrap();
        let run = std::process::Command::new("node")
            .arg("run.js")
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let stdout = String::from_utf8_lossy(&run.stdout);
        let results: Vec<&str> = stdout.lines().collect();
        assert_eq!(results.len(), 18, "{stdout}");
        let accepted = [
            "0",
            "-1",
            "9007199254740991",
            "-9007199254740991",
            "9007199254740990",
            "1000000000000000",
            "8999999999999999",
        ];
        for (index, value) in accepted.iter().enumerate() {
            // The decoded value and `encodeInt` of it (a JSON string) are exact.
            assert_eq!(results[index], format!("OK {value} \"{value}\""));
        }
        for (offset, result) in results[accepted.len()..].iter().enumerate() {
            assert_eq!(*result, "FAIL", "rejected input #{offset} was accepted");
        }
    }

    /// A generation that fails leaves the previous output untouched.
    #[test]
    fn failed_generation_keeps_the_previous_output() {
        let root = std::env::temp_dir().join(format!("bindc-generate-{}", std::process::id()));
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(
            root.join("app/main.bd"),
            "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\nserver fun getCustomer(id: CustomerId): Customer effects [db.read] = db.customers.require(id)\nclient component CustomerPage(id: CustomerId) {\n    val customer = server.getCustomer(id)\n    UI { Text(customer.name) }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("schema.csv"),
            "table,model,primary_key,column,type,nullable,required,has_default,engine\ncustomers,Customer,id,id,CustomerId,false,true,true,postgresql\ncustomers,Customer,id,name,String,false,true,false,postgresql\n",
        )
        .unwrap();
        fs::write(
            root.join("bind.toml"),
            "[project]\nname = \"test\"\nsource = \"app\"\n[database.main]\nschema = \"schema.csv\"\nverification = \"static\"\n[target.aws-lambda]\nclient = \"elm\"\nserver = \"rust\"\ndeployment = \"aws-lambda\"\ncapabilities = [\"ui.dom\", \"server.http\", \"db.read\"]\n",
        )
        .unwrap();
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let result = crate::compiler::verify(&config, "aws-lambda");
        let mut app = result
            .application
            .unwrap_or_else(|| panic!("{:?}", result.diagnostics));
        let output = root.join("out");
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("previous.txt"), "kept").unwrap();
        // Simulate a backend failure: an endpoint reading an `environment` value, which
        // `verify` would have rejected (E8310).
        let endpoint = app
            .core
            .functions
            .iter_mut()
            .find(|function| function.endpoint)
            .unwrap();
        let ty = endpoint.body.ty.clone();
        endpoint.body = crate::core::ir::Expr {
            ty,
            kind: crate::core::ir::ExprKind::Environment("databaseUrl".to_string()),
        };
        let error = generate(&app, &config, Some(&output)).unwrap_err();
        assert!(error.contains("environment"), "{error}");
        assert_eq!(
            fs::read_to_string(output.join("previous.txt")).unwrap(),
            "kept"
        );
        assert!(!output.join("Cargo.toml").exists());
        fs::remove_dir_all(root).unwrap();
    }

    /// `generate` on `examples/commerce` (the repository's `bind.toml`) for both targets, then
    /// `cargo clippy --all-targets -- -D warnings` and `cargo test` in the generated crate.
    #[test]
    #[ignore = "generates and builds Rust crates; run with `cargo test -- --ignored`"]
    fn generated_commerce_builds_without_warnings() {
        if std::env::var_os("DATABASE_URL").is_none() {
            // SAFETY: read only through `std::env`, whose accesses take the same lock.
            unsafe { std::env::set_var("DATABASE_URL", "postgresql://bind:bind@localhost/bind") };
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        for target in ["aws-lambda", "vercel"] {
            let result = crate::compiler::verify(&config, target);
            let app = result
                .application
                .unwrap_or_else(|| panic!("{target}: {:?}", result.diagnostics));
            let output = root
                .join("target/bind-codegen-tests")
                .join(format!("commerce-{target}"));
            let generated = generate(&app, &config, Some(&output)).unwrap();
            fs::copy(
                root.join("runtime/bind-runtime/Cargo.lock"),
                generated.join("Cargo.lock"),
            )
            .unwrap();
            crate::backend::rust::tests::cargo(
                &generated,
                &["clippy", "--quiet", "--all-targets", "--", "-D", "warnings"],
            );
            crate::backend::rust::tests::cargo(&generated, &["test", "--quiet"]);
        }
    }

    #[test]
    fn parses_nested_types() {
        let parsed = ParsedType::parse("List<List<String>>").unwrap();
        assert_eq!(parsed.render(), "List<List<String>>");
    }

    #[test]
    fn converts_identifiers() {
        assert_eq!(snake_case("getCustomer"), "get_customer");
        assert_eq!(pascal_case("get_customer"), "GetCustomer");
        assert_eq!(lower_camel("CustomerId"), "customerId");
    }

    #[test]
    fn parses_certified_static_ui_tree() {
        let (tokens, diagnostics) = lex(
            "site.bd",
            r#"UI { Section("hero", "top") { Heading(1, "title") { Text("Bind") } Link("/docs", "cta") { Text("Read") } } }"#,
        );
        assert!(diagnostics.is_empty());
        let open = tokens.iter().position(|token| token.text == "{").unwrap();
        let (nodes, _) = parse_ui_nodes(&tokens, open + 1, true).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "Section");
        assert_eq!(nodes[0].children.len(), 2);
        assert_eq!(nodes[0].children[0].name, "Heading");
        assert_eq!(nodes[0].children[1].name, "Link");
    }
}

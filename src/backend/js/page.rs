//! The `web/` folder of a web target: the page shell shared by both modes, the static page
//! pre-rendered at compile time, and the split mode's page (`index.html` with the loading
//! screen, `app.js` with the boot code, the runtime) — all from the [`FrontendPlan`].
//!
//! | Page | `ssr` | `split` |
//! |---|---|---|
//! | static (no parameters, no server call, evaluated by `core::eval`) | `index.html` pre-rendered, no JS | the same |
//! | dynamic | rendered by the server at `GET /`; `web/` holds the stylesheet | `index.html` + `app.js` + `bind-runtime.js` |
//!
//! `styles.css` is always written; every page links it as [`STYLESHEET`].

use crate::compiler::VerifiedApplication;
use crate::config::{Mode, ProjectConfig, TargetConfig};
use crate::core::display::{UiArg, UiTree, escape_html, render_html_value};
use crate::core::eval::{Value, eval_function};
use crate::core::ir::{CoreProgram, UiNode};

use super::names::js_string;
use super::plan::{FrontendPlan, frontend_plan};

/// The stylesheet URL of every page (the shell's `<link>`).
pub const STYLESHEET: &str = "/styles.css";

/// The text of the loading screen: the runtimes' `TEXT_LOADING` / `TEXTS.loading`.
pub const LOADING_TEXT: &str = "Loading…";

/// The name `app.js` exports its page object (`{ readParams, boot }`) under, unless the
/// component itself has that name (then `_` is appended until they differ).
pub const PAGE_EXPORT: &str = "bind:page";

/// The single page shell of both modes (the runtimes' `page_html` / `pageHtml`): a full HTML
/// document with the title, the stylesheet link and `body` (already HTML) inside the one
/// `<div class="bind-site">`.
pub fn page_html(title: &str, stylesheet: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><link rel=\"stylesheet\" href=\"{}\"></head>\
         <body><div class=\"bind-site\">{body}</div></body></html>",
        escape_html(title),
        escape_html(stylesheet)
    )
}

/// The loading screen's HTML (the JS runtime's `LOADING`).
pub fn loading_html() -> String {
    let text = UiTree {
        node: UiNode::Text,
        args: vec![UiArg::Str(LOADING_TEXT.to_string())],
        children: Vec::new(),
    };
    let paragraph = UiTree {
        node: UiNode::Paragraph,
        args: Vec::new(),
        children: vec![text],
    };
    render_html_value(&UiTree {
        node: UiNode::Main,
        args: vec![UiArg::Str("bind-loading".to_string())],
        children: vec![paragraph],
    })
}

/// The HTML of the page's component evaluated at compile time, when it is static: no
/// parameters, no server call, and `core::eval` evaluates it to a UI tree. Otherwise (it fails
/// or uses what the evaluator does not model) `None`: the page is rendered at run time, where a
/// failure shows its error screen with its status.
pub fn prerender(core: &CoreProgram, plan: &FrontendPlan) -> Option<String> {
    if !plan.may_be_static() {
        return None;
    }
    match eval_function(core, &plan.component, Vec::new()) {
        Ok(value @ Value::Ui { .. }) => Some(value.render()),
        _ => None,
    }
}

/// The default stylesheet (no `[web].stylesheet`): the page wrapper and the standard screens.
pub const DEFAULT_CSS: &str = ":root { color-scheme: light dark; font-family: ui-sans-serif, system-ui, sans-serif; line-height: 1.5; }
body { margin: 0; }
.bind-site { max-width: 48rem; margin: 0 auto; padding: 2rem 1rem; }
.bind-loading { opacity: .7; }
.bind-error h1 { font-size: 1.5rem; margin: 0 0 .5rem; }
.bind-error p { margin: 0; }
";

/// The stylesheet of the project: `[web].stylesheet` or [`DEFAULT_CSS`].
pub fn stylesheet(config: &ProjectConfig) -> Result<String, String> {
    match &config.web_stylesheet {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("Cannot read web stylesheet {}: {error}", path.display())),
        None => Ok(DEFAULT_CSS.to_string()),
    }
}

/// The frontend of `app` on `target`: its plan, whether the page is static (pre-rendered) and
/// the files of `web/` (relative path, content; see the module table).
#[derive(Clone, Debug)]
pub struct WebBuild {
    pub plan: FrontendPlan,
    pub is_static: bool,
    pub files: Vec<(String, String)>,
}

/// The files of `web/` for `app` on `target`; see the module table.
#[cfg(test)]
pub fn web_files(
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target: &TargetConfig,
) -> Result<Vec<(String, String)>, String> {
    web_build(app, config, target).map(|build| build.files)
}

/// The [`WebBuild`] of `app` on `target`. Fails only for what `verify` reports (E8311, the
/// same plan) or an unreadable stylesheet.
pub fn web_build(
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target: &TargetConfig,
) -> Result<WebBuild, String> {
    let plan = frontend_plan(&app.core, &app.program, &app.target, target).map_err(|problems| {
        problems
            .iter()
            .map(|problem| problem.message())
            .collect::<Vec<_>>()
            .join(" ")
    })?;
    let mut files = Vec::new();
    let prerendered = prerender(&app.core, &plan);
    if let Some(body) = &prerendered {
        files.push((
            "web/index.html".to_string(),
            page_html(&config.web_title, STYLESHEET, body),
        ));
    } else if target.mode == Mode::Split {
        let export = page_export(&plan);
        files.push((
            "web/index.html".to_string(),
            split_index(&config.web_title, &export),
        ));
        files.push((
            "web/app.js".to_string(),
            app_module(&app.core, &plan, &target.api_url, &export),
        ));
        files.push((
            "web/bind-runtime.js".to_string(),
            super::RUNTIME.to_string(),
        ));
    }
    files.push(("web/styles.css".to_string(), stylesheet(config)?));
    Ok(WebBuild {
        plan,
        is_static: prerendered.is_some(),
        files,
    })
}

fn page_export(plan: &FrontendPlan) -> String {
    let mut export = PAGE_EXPORT.to_string();
    while export == plan.component {
        export.push('_');
    }
    export
}

/// The split page before its JS runs: the shell with the loading screen, then the module
/// script that boots the page into the shell's `div.bind-site`.
fn split_index(title: &str, export: &str) -> String {
    let page = page_html(title, STYLESHEET, &loading_html());
    let script = format!(
        "<script type=\"module\">import {{ {} as page }} from \"/app.js\"; page.boot(document.querySelector(\"div.bind-site\"), window.location.search, document);</script>",
        js_string(export)
    );
    match page.strip_suffix("</body></html>") {
        Some(start) => format!("{start}{script}</body></html>"),
        None => page,
    }
}

/// `app.js` of the split page: the component's module (`render_app_with`) and the page object
/// exported as `export`: `readParams(search)` reads the component's parameters with the strict
/// text decoders (absent, repeated or invalid: `BindError("BadParameter", name)`, as the SSR's
/// 400 page) and `boot(container, search, document)` shows the loading screen, runs the
/// component and renders its UI or `rt.errorScreen` of the failure into `container`.
pub fn app_module(core: &CoreProgram, plan: &FrontendPlan, api_url: &str, export: &str) -> String {
    let module = super::render_app_with(core, std::slice::from_ref(&plan.component), api_url);
    let specs: Vec<String> = plan
        .params
        .iter()
        .map(|param| {
            let decoder = match param.base.as_str() {
                "Int" => "textInt",
                "UUID" => "textUuid",
                "Date" => "textDate",
                "DateTime" => "textDateTime",
                "Instant" => "textInstant",
                _ => "textString",
            };
            format!("[{}, rt.{decoder}]", js_string(&param.name))
        })
        .collect();
    let mut arguments = vec!["rt.newCtx()".to_string()];
    arguments.extend(
        plan.params
            .iter()
            .map(|param| format!("params[{}]", js_string(&param.name))),
    );
    let boot = format!(
        "
// The page (split mode): reads the component's parameters from the query string, shows the
// loading screen, runs the component and renders its UI, or the error screen of the failure,
// into the page's `div.bind-site` (index.html calls `boot`).
function readPageParams(search) {{
return rt.readParams(search, [{specs}]);
}}

async function bootPage(container, search, document) {{
container.replaceChildren(...rt.render(rt.LOADING, document));
let tree;
try {{
const params = readPageParams(search);
tree = await {component}({arguments});
}} catch (error) {{
tree = rt.errorScreen(error).tree;
}}
container.replaceChildren(...rt.render(tree, document));
}}

const page = Object.freeze({{ readParams: readPageParams, boot: bootPage }});
export {{ page as {export} }};
",
        specs = specs.join(", "),
        component = super::names::js_value(&plan.component),
        arguments = arguments.join(", "),
        export = js_string(export),
    );
    crate::backend::rust::indent(&format!("{module}{boot}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_matches_the_runtimes_page_vectors() {
        let vectors = include_str!("../../../runtime/vectors/pages.json");
        let json = |html: &str| html.replace('\\', "\\\\").replace('"', "\\\"");
        for (title, stylesheet, body) in [
            ("Home", "/styles.css", "<p>hi</p>"),
            ("A & B <i>", "/s.css?x=\"1\"&y='2'", "<main>x</main>"),
            ("", "", ""),
        ] {
            let html = page_html(title, stylesheet, body);
            assert!(vectors.contains(&json(&html)), "{html}");
        }
    }

    #[test]
    fn the_loading_screen_uses_the_runtimes_text() {
        let runtime = include_str!("../../../runtime/bind-runtime/src/lib.rs");
        assert!(runtime.contains(&format!(
            "pub const TEXT_LOADING: &str = \"{LOADING_TEXT}\";"
        )));
        assert_eq!(
            loading_html(),
            "<main class=\"bind-loading\"><p>Loading…</p></main>"
        );
    }

    #[test]
    fn the_split_page_is_the_shell_with_the_loading_screen_and_the_boot_script() {
        let page = split_index("Shop & co", "bind:page");
        assert_eq!(
            page,
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Shop &amp; co</title><link rel=\"stylesheet\" href=\"/styles.css\"></head><body><div class=\"bind-site\"><main class=\"bind-loading\"><p>Loading…</p></main></div><script type=\"module\">import { \"bind:page\" as page } from \"/app.js\"; page.boot(document.querySelector(\"div.bind-site\"), window.location.search, document);</script></body></html>"
        );
    }

    fn plan(source: &str) -> (CoreProgram, FrontendPlan) {
        let core = crate::core::lower::tests::compile(source);
        let component = core
            .functions
            .iter()
            .find(|function| function.domain == crate::core::ir::Domain::Client)
            .unwrap();
        let params = component
            .params
            .iter()
            .map(|(_, name, ty)| super::super::plan::PageParam {
                name: name.clone(),
                base: crate::types::ty::display(ty),
            })
            .collect();
        let plan = FrontendPlan {
            component: component.name.clone(),
            params,
            uses_server: false,
        };
        (core, plan)
    }

    #[test]
    fn static_components_are_prerendered_when_the_evaluator_can() {
        let (core, page) = plan(
            "fun nome(): String = \"<Bind>\"\nclient component C() {\n UI {\n Section(\"hero\") { Heading(1) { Text(nome()) } }\n }\n}\n",
        );
        assert_eq!(
            prerender(&core, &page).as_deref(),
            Some("<section class=\"hero\"><h1>&lt;Bind&gt;</h1></section>")
        );
        // Recursion past the limit fails at run time: rendered there, with its error screen.
        let (core, page) = plan(
            "fun fundo(n: Int): Int = if (n == 0) 0 else fundo(n - 1) + 1\nclient component C() {\n UI {\n Text(fundo(20000))\n }\n}\n",
        );
        assert_eq!(prerender(&core, &page), None);
        // A parameter: read from the URL at run time.
        let (core, page) = plan("client component C(n: Int) {\n UI {\n Text(n)\n }\n}\n");
        assert_eq!(prerender(&core, &page), None);
    }

    #[test]
    fn the_page_object_reads_parameters_and_boots_the_component() {
        let (core, page) =
            plan("client component C(id: UUID, nome: String) {\n UI {\n Text(nome)\n }\n}\n");
        let module = app_module(&core, &page, "/api", "bind:page");
        assert!(module.contains(
            "return rt.readParams(search, [[\"id\", rt.textUuid], [\"nome\", rt.textString]]);"
        ));
        assert!(
            module.contains("tree = await u_C(rt.newCtx(), params[\"id\"], params[\"nome\"]);")
        );
        assert!(module.contains("tree = rt.errorScreen(error).tree;"));
        assert!(
            module.ends_with("export { page as \"bind:page\" };\n"),
            "{module}"
        );
        let named = FrontendPlan {
            component: "bind:page".to_string(),
            ..page
        };
        assert_eq!(page_export(&named), "bind:page_");
    }

    /// A verified project in a temporary directory, removed on drop.
    struct Project {
        root: std::path::PathBuf,
        config: ProjectConfig,
        app: VerifiedApplication,
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let path = entry.unwrap().path();
            let target = to.join(path.file_name().unwrap());
            if path.is_dir() {
                copy_tree(&path, &target);
            } else {
                std::fs::copy(&path, &target).unwrap();
            }
        }
    }

    /// `files` (relative path, content) written to a fresh temporary project, verified on `target`.
    fn project(name: &str, files: &[(&str, String)], target: &str) -> Project {
        let root = std::env::temp_dir().join(format!("bindc-page-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let result = crate::compiler::verify(&config, target);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let app = result.application.unwrap();
        Project { root, config, app }
    }

    /// The site (`../website`, not modified: a temporary copy with `client = "web"` and
    /// `mode = "split"`) pre-renders exactly the HTML its former Elm client rendered
    /// (`tests/codegen/website.html.expected`, recorded before the Elm generator was removed).
    #[test]
    fn the_website_prerenders_the_html_of_the_former_elm_client() {
        let site = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../website");
        if !site.join("src/site.bd").exists() {
            eprintln!("skipped: ../website not found");
            return;
        }
        let manifest = std::fs::read_to_string(site.join("bind.toml"))
            .unwrap()
            .replace("client = \"elm\"", "client = \"web\"\nmode = \"split\"");
        assert!(manifest.contains("mode = \"split\""));
        let root = std::env::temp_dir().join(format!("bindc-page-website-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        copy_tree(&site.join("src"), &root.join("src"));
        std::fs::copy(site.join("styles.css"), root.join("styles.css")).unwrap();
        std::fs::write(root.join("bind.toml"), &manifest).unwrap();
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let result = crate::compiler::verify(&config, "vercel");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let app = result.application.unwrap();
        let files = web_files(&app, &config, &config.targets["vercel"]).unwrap();
        let paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["web/index.html", "web/styles.css"]);
        let expected = include_str!("../../../tests/codegen/website.html.expected");
        let inner = expected
            .strip_prefix("<div class=\"bind-site\">")
            .and_then(|rest| rest.strip_suffix("</div>"))
            .unwrap();
        assert_eq!(files[0].1, page_html(&config.web_title, STYLESHEET, inner));
        assert_eq!(
            files[1].1,
            std::fs::read_to_string(site.join("styles.css")).unwrap()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    const SCHEMA: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine\ncustomers,Customer,id,id,CustomerId,false,true,true,postgresql\ncustomers,Customer,id,name,String,false,true,false,postgresql\n";

    fn split_project(name: &str, source: &str) -> Project {
        project(
            name,
            &[
                ("app/main.bd", source.to_string()),
                ("schema.csv", SCHEMA.to_string()),
                (
                    "bind.toml",
                    "[project]\nname = \"t\"\nsource = \"app\"\n[database.main]\nschema = \"schema.csv\"\nverification = \"static\"\n[target.t]\nclient = \"web\"\nserver = \"rust\"\ndeployment = \"vercel\"\nmode = \"split\"\ncapabilities = [\"ui.dom\", \"server.http\", \"db.read\"]\n".to_string(),
                ),
            ],
            "t",
        )
    }

    /// Review Focus 1 in the split mode: the generated `app.js` reads its parameters with
    /// `rt.readParams` as the SSR does (`%XX` and `+` decoded; absent, repeated or invalid is
    /// `BadParameter`), and `boot` renders the component or the SSR's error screen.
    #[test]
    #[ignore = "needs node; run with `cargo test backend::js -- --ignored`"]
    fn the_split_page_reads_its_parameters_like_the_ssr() {
        if !node_available() {
            eprintln!("skipped: node not found");
            return;
        }
        let customer = split_project(
            "node-customer",
            "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\nserver fun getCustomer(id: CustomerId): Customer effects [db.read] = db.customers.require(id)\nclient component CustomerPage(id: CustomerId) {\n    val customer = server.getCustomer(id)\n    UI { Text(customer.name) }\n}\n",
        );
        let greeting = split_project(
            "node-greeting",
            "client component Ola(nome: String) {\n    UI { Paragraph { Text(\"Olá, \" + nome) } }\n}\n",
        );
        let folder = std::env::temp_dir().join(format!("bindc-page-node-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        for (project, directory) in [(&customer, "customer"), (&greeting, "greeting")] {
            let target = &project.config.targets["t"];
            for (path, content) in web_files(&project.app, &project.config, target).unwrap() {
                let path = folder.join(directory).join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            }
        }
        std::fs::write(
            folder.join("dom.mjs"),
            include_str!("../../../runtime/bind-runtime-js/tests/dom.mjs"),
        )
        .unwrap();
        std::fs::write(
            folder.join("check.mjs"),
            r#"
import { "bind:page" as customer } from "./customer/web/app.js";
import { "bind:page" as greeting } from "./greeting/web/app.js";
import { createDocument, serialize } from "./dom.mjs";
const show = (read) => {
  try {
    return JSON.stringify({ ...read() });
  } catch (error) {
    // Each page has its own copy of the runtime, hence its own BindError class.
    return error?.name === "BindError" ? `${error.kind}:${error.message}` : `exception ${error}`;
  }
};
const uuid = "1b4e28ba-2fa1-4d3b-a3f5-ef19ae7a0c12";
const lines = [
  show(() => customer.readParams(`?id=${uuid}`)),
  show(() => customer.readParams("?id=a&id=b")),
  show(() => customer.readParams(`?id=${uuid}&id=${uuid}`)),
  show(() => customer.readParams("?outro=1")),
  show(() => customer.readParams("?id=1B4E28BA-2FA1-4D3B-A3F5-EF19AE7A0C12")),
  show(() => greeting.readParams("?nome=Jo%C3%A3o+Silva")),
  show(() => greeting.readParams("?nome=%E0%A4%A")),
];
const boot = async (page, search) => {
  const document = createDocument();
  const container = document.createElement("div");
  await page.boot(container, search, document);
  return container.childNodes.map(serialize).join("");
};
const requests = [];
globalThis.fetch = async (url, init) => {
  requests.push(`${url} ${init.body}`);
  const id = JSON.parse(init.body).id;
  if (id === uuid) return new Response(JSON.stringify({ id, name: "Ana <admin>" }), { status: 200 });
  return new Response('{"error":"not found"}', { status: 404 });
};
lines.push(await boot(customer, `?id=${uuid}`));
lines.push(await boot(customer, "?id=00000000-0000-4000-8000-000000000000"));
lines.push(await boot(customer, "?id=a&id=b"));
lines.push(await boot(greeting, "?nome=Jo%C3%A3o+Silva"));
lines.push(...requests);
console.log(lines.join("\n"));
"#,
        )
        .unwrap();
        let run = std::process::Command::new("node")
            .arg("check.mjs")
            .current_dir(&folder)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&run.stdout).to_string();
        assert!(
            run.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let error = |title: &str, message: &str| {
            format!("<main class=\"bind-error\"><h1>{title}</h1><p>{message}</p></main>")
        };
        let expected = [
            "{\"id\":\"1b4e28ba-2fa1-4d3b-a3f5-ef19ae7a0c12\"}".to_string(),
            "BadParameter:id".to_string(),
            "BadParameter:id".to_string(),
            "BadParameter:id".to_string(),
            "BadParameter:id".to_string(),
            "{\"nome\":\"João Silva\"}".to_string(),
            "BadParameter:nome".to_string(),
            "Ana &lt;admin&gt;".to_string(),
            error("Page not found", "not found"),
            error("Bad request", "invalid parameter: id"),
            "<p>Olá, João Silva</p>".to_string(),
            "/api/getCustomer {\"id\":\"1b4e28ba-2fa1-4d3b-a3f5-ef19ae7a0c12\"}".to_string(),
            "/api/getCustomer {\"id\":\"00000000-0000-4000-8000-000000000000\"}".to_string(),
        ];
        assert_eq!(stdout.lines().collect::<Vec<_>>(), expected);
        std::fs::remove_dir_all(&folder).unwrap();
    }
}

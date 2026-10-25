//! Template permissions: what a template declares it needs, and the static
//! checks that run before any of its code does.
//!
//! A permission set maps a permission id (`send_http`, `read_event_data`, ...)
//! to its parameters. The sandbox enforces the parameters when the API is
//! called; [`check_template`] rejects, at load time, a template that uses an
//! API it holds no permission for, requires an API that doesn't exist, or
//! reads an identifier it never declared.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_ast::ast::{Argument, Expression};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType};
use serde_json::{Value, json};

/// API name to the permission it needs, `null` for open APIs. One list for the
/// load-time check and the sandbox.
pub const API_PERMISSIONS: &str = include_str!("sandbox/api_permissions.json");

/// Hosts a `google` URL permission allows.
const GOOGLE_URLS: &[&str] = &[
    "https://*.google.com/*",
    "https://*.google-analytics.com/*",
    "https://*.googleapis.com/*",
    "https://*.googletagmanager.com/*",
    "https://*.doubleclick.net/*",
];

/// Names a template may use without `require`: the language's own constants.
const OPEN_GLOBALS: &[&str] = &["undefined", "NaN", "Infinity"];

/// Permission id to normalized parameters.
pub type Permissions = BTreeMap<String, Value>;

fn api_table() -> HashMap<String, Option<String>> {
    serde_json::from_str(API_PERMISSIONS).expect("api_permissions.json is valid")
}

fn list_param(id: &str, params: &Value, key: &str) -> Result<(), String> {
    match params.get(key).and_then(Value::as_array) {
        Some(a) if a.iter().all(Value::is_string) => Ok(()),
        _ => Err(format!(
            "permission {id}: `{key}` must be a list of strings"
        )),
    }
}

/// Validates a permission set written in a container file.
pub fn from_json(v: &Value) -> Result<Permissions, String> {
    let obj = v
        .as_object()
        .ok_or("permissions must be an object of permission ids")?;
    let mut out = Permissions::new();
    for (id, params) in obj {
        match id.as_str() {
            "send_http" => list_param(id, params, "urls")?,
            "read_event_data" => list_param(id, params, "keys")?,
            "get_cookies" | "set_cookies" => list_param(id, params, "names")?,
            // `project.dataset.table` patterns, `*` matching anything.
            "access_bigquery" => list_param(id, params, "tables")?,
            "logging"
            | "access_response"
            | "read_request"
            | "return_response"
            | "run_container"
            | "read_container_data"
            | "access_template_storage" => {}
            other => return Err(format!("unknown permission {other}")),
        }
        out.insert(id.clone(), params.clone());
    }
    Ok(out)
}

/// A template exported from Google's template editor or gallery (`.tpl`).
pub struct TplFile {
    pub code: String,
    pub permissions: Permissions,
}

pub fn parse_tpl(text: &str) -> Result<TplFile, String> {
    let mut sections: HashMap<&str, String> = HashMap::new();
    let mut current: Option<&str> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.len() > 6 && t.starts_with("___") && t.ends_with("___") {
            current = Some(&t[3..t.len() - 3]);
            sections.entry(current.unwrap()).or_default();
        } else if let Some(name) = current {
            let s = sections.get_mut(name).unwrap();
            s.push_str(line);
            s.push('\n');
        }
    }
    let code = sections
        .remove("SANDBOXED_JS_FOR_SERVER")
        .ok_or("no ___SANDBOXED_JS_FOR_SERVER___ section: not a server template")?;
    let raw = sections.remove("SERVER_PERMISSIONS").unwrap_or_default();
    let list: Vec<Value> = if raw.trim().is_empty() {
        vec![]
    } else {
        serde_json::from_str(&raw).map_err(|e| format!("___SERVER_PERMISSIONS___: {e}"))?
    };
    let mut permissions = Permissions::new();
    for item in &list {
        let key = &item["instance"]["key"]["publicId"];
        let Some(id) = key.as_str() else { continue };
        let params = tpl_params(&item["instance"]["param"]);
        let norm = match id {
            "send_http" => match tpl_str(&params, "allowedUrls") {
                "any" => Some(json!({ "urls": ["*"] })),
                "google" => Some(json!({ "urls": GOOGLE_URLS })),
                _ => Some(json!({ "urls": tpl_list(&params, "urls") })),
            },
            "read_event_data" => match tpl_str(&params, "eventDataAccess") {
                "any" => Some(json!({ "keys": ["*"] })),
                _ => Some(json!({ "keys": tpl_list(&params, "keyPatterns") })),
            },
            "get_cookies" => match tpl_str(&params, "cookieAccess") {
                "any" => Some(json!({ "names": ["*"] })),
                _ => Some(json!({ "names": tpl_list(&params, "cookieNames") })),
            },
            "set_cookies" => Some(json!({ "names": tpl_cookie_names(&params) })),
            "access_bigquery" => Some(json!({ "tables": tpl_bigquery_tables(&params) })),
            "logging"
            | "access_response"
            | "read_request"
            | "return_response"
            | "run_container"
            | "read_container_data"
            | "access_template_storage" => Some(json!({})),
            // Permissions for APIs this server doesn't have.
            _ => None,
        };
        if let Some(n) = norm {
            permissions.insert(id.to_string(), n);
        }
    }
    Ok(TplFile { code, permissions })
}

/// A `.tpl` permission's `param` list as key to value node.
fn tpl_params(param: &Value) -> HashMap<String, Value> {
    param
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["key"].as_str()?.to_string(), p["value"].clone())))
        .collect()
}

fn tpl_str<'a>(params: &'a HashMap<String, Value>, key: &str) -> &'a str {
    params
        .get(key)
        .and_then(|v| v["string"].as_str())
        .unwrap_or("")
}

fn tpl_list(params: &HashMap<String, Value>, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v["listItem"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|i| i["string"].as_str().map(str::to_string))
        .collect()
}

/// The cookie names in `set_cookies`' `allowedCookies` table.
fn tpl_cookie_names(params: &HashMap<String, Value>) -> Vec<String> {
    params
        .get("allowedCookies")
        .and_then(|v| v["listItem"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|row| {
            row["mapKey"]
                .as_array()?
                .iter()
                .zip(row["mapValue"].as_array()?)
                .find(|(k, _)| k["string"] == "name")
                .and_then(|(_, v)| v["string"].as_str().map(str::to_string))
        })
        .collect()
}

/// The tables in `access_bigquery`'s `allowedTables` table that may be
/// written, as `project.dataset.table`. An empty project id matches any.
fn tpl_bigquery_tables(params: &HashMap<String, Value>) -> Vec<String> {
    params
        .get("allowedTables")
        .and_then(|v| v["listItem"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let field = |name: &str| -> Option<&str> {
                row["mapKey"]
                    .as_array()?
                    .iter()
                    .zip(row["mapValue"].as_array()?)
                    .find(|(k, _)| k["string"] == name)
                    .and_then(|(_, v)| v["string"].as_str())
            };
            if field("operation").is_some_and(|op| op != "write" && op != "read_write") {
                return None;
            }
            let project = field("projectId").filter(|p| !p.is_empty()).unwrap_or("*");
            Some(format!(
                "{project}.{}.{}",
                field("datasetId")?,
                field("tableId")?
            ))
        })
        .collect()
}

/// Checks a template's source before it runs: it must parse, read no
/// identifier it didn't declare, `require` only APIs that exist and, when
/// `permissions` is given, only those it holds a permission for.
pub fn check_template(src: &str, permissions: Option<&Permissions>) -> Result<(), String> {
    // The sandbox runs a template as the body of this function.
    let head = "(function (require, data) { 'use strict';\n";
    let wrapped = format!("{head}{src}\n}})");
    let line_of = |offset: u32| {
        wrapped[..(offset as usize).min(wrapped.len())]
            .matches('\n')
            .count()
    };

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &wrapped, SourceType::script()).parse();
    if let Some(e) = parsed.diagnostics.first() {
        return Err(format!("syntax error: {e}"));
    }
    let sem = SemanticBuilder::new()
        .with_build_nodes(true)
        .build(&parsed.program);
    if let Some(e) = sem.diagnostics.first() {
        return Err(format!("{e}"));
    }
    let semantic = sem.semantic;

    // Identifiers that resolve to nothing the template declared.
    let scoping = semantic.scoping();
    let mut undeclared: Vec<(usize, String)> = vec![];
    for (name, refs) in scoping.root_unresolved_references() {
        let name = name.as_str();
        if OPEN_GLOBALS.contains(&name) {
            continue;
        }
        for id in refs {
            let span = semantic
                .nodes()
                .get_node(scoping.get_reference(*id).node_id())
                .span();
            undeclared.push((line_of(span.start), name.to_string()));
        }
    }
    undeclared.sort();
    if let Some((line, name)) = undeclared.first() {
        return Err(format!(
            "line {}: `{name}` is not declared. Load APIs with require('{name}') or declare the variable",
            line
        ));
    }

    // Everything the template requires.
    let table = api_table();
    let mut needed: BTreeSet<(String, String)> = BTreeSet::new();
    for node in semantic.nodes().iter() {
        let AstKind::CallExpression(call) = node.kind() else {
            continue;
        };
        let Expression::Identifier(callee) = &call.callee else {
            continue;
        };
        if callee.name != "require" {
            continue;
        }
        let line = line_of(call.span.start);
        let Some(Argument::StringLiteral(lit)) = call.arguments.first() else {
            return Err(format!("line {line}: require() needs a literal API name"));
        };
        let api = lit.value.as_str();
        let Some(perm) = table.get(api) else {
            return Err(format!("line {line}: require('{api}'): no such API"));
        };
        if let (Some(perm), Some(_)) = (perm, permissions) {
            needed.insert((perm.clone(), api.to_string()));
        }
    }
    if let Some(held) = permissions {
        for (perm, api) in &needed {
            if !held.contains_key(perm) {
                return Err(format!(
                    "require('{api}') needs the `{perm}` permission, which the template doesn't declare"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms(v: Value) -> Permissions {
        from_json(&v).unwrap()
    }

    #[test]
    fn clean_template_passes() {
        let src = "const log = require('logToConsole');\nlog('x');\ndata.gtmOnSuccess();";
        assert!(check_template(src, Some(&perms(json!({"logging": {}})))).is_ok());
    }

    #[test]
    fn undeclared_identifier_is_named_with_its_line() {
        let src = "const a = 1;\nconsole.log(a);\ndata.gtmOnSuccess();";
        let e = check_template(src, None).unwrap_err();
        assert!(e.contains("line 2") && e.contains("`console`"), "{e}");
    }

    #[test]
    fn locals_params_and_properties_are_not_flagged() {
        let src = "function f(a) { return a.window + b; }\nvar b = 1;\nlet o = { document: f(2) };\ntry { f(1); } catch (err) { o.err = err; }\n[1].map((x) => x + 1);\ndata.gtmOnSuccess();";
        assert!(check_template(src, None).is_ok());
    }

    #[test]
    fn missing_permission_is_rejected() {
        let src = "const send = require('sendHttpRequest');";
        let e = check_template(src, Some(&perms(json!({})))).unwrap_err();
        assert!(e.contains("send_http"), "{e}");
        assert!(check_template(src, None).is_ok());
    }

    #[test]
    fn unknown_api_and_dynamic_require_are_rejected() {
        assert!(check_template("require('fetch');", None).is_err());
        assert!(check_template("const n = 'x'; require(n);", None).is_err());
    }

    #[test]
    fn syntax_errors_are_rejected() {
        assert!(check_template("const = ;", None).is_err());
    }

    #[test]
    fn tpl_permissions_are_read() {
        let tpl = r#"___TERMS_OF_SERVICE___

x

___SANDBOXED_JS_FOR_SERVER___

const log = require('logToConsole');
log(1);
data.gtmOnSuccess();

___SERVER_PERMISSIONS___

[
  {"instance":{"key":{"publicId":"logging","versionId":"1"},"param":[]},"isRequired":true},
  {"instance":{"key":{"publicId":"send_http","versionId":"1"},"param":[
    {"key":"allowedUrls","value":{"type":1,"string":"specific"}},
    {"key":"urls","value":{"type":2,"listItem":[{"type":1,"string":"https://example.com/*"}]}}]},"isRequired":true},
  {"instance":{"key":{"publicId":"access_bigquery","versionId":"1"},"param":[
    {"key":"allowedTables","value":{"type":2,"listItem":[
      {"type":3,"mapKey":[{"type":1,"string":"projectId"},{"type":1,"string":"datasetId"},{"type":1,"string":"tableId"},{"type":1,"string":"operation"}],
       "mapValue":[{"type":1,"string":"my-project"},{"type":1,"string":"analytics"},{"type":1,"string":"events"},{"type":1,"string":"write"}]},
      {"type":3,"mapKey":[{"type":1,"string":"projectId"},{"type":1,"string":"datasetId"},{"type":1,"string":"tableId"},{"type":1,"string":"operation"}],
       "mapValue":[{"type":1,"string":""},{"type":1,"string":"analytics"},{"type":1,"string":"orders"},{"type":1,"string":"write"}]},
      {"type":3,"mapKey":[{"type":1,"string":"projectId"},{"type":1,"string":"datasetId"},{"type":1,"string":"tableId"},{"type":1,"string":"operation"}],
       "mapValue":[{"type":1,"string":"my-project"},{"type":1,"string":"analytics"},{"type":1,"string":"users"},{"type":1,"string":"read"}]}]}}]},"isRequired":true},
  {"instance":{"key":{"publicId":"access_firestore","versionId":"1"},"param":[]},"isRequired":true}
]
"#;
        let t = parse_tpl(tpl).unwrap();
        assert!(t.code.contains("require('logToConsole')"));
        assert_eq!(
            t.permissions["send_http"],
            json!({"urls": ["https://example.com/*"]})
        );
        assert!(t.permissions.contains_key("logging"));
        // Tables it may write; the empty project matches any, and the
        // read-only one is left out.
        assert_eq!(
            t.permissions["access_bigquery"],
            json!({"tables": ["my-project.analytics.events", "*.analytics.orders"]})
        );
        assert!(!t.permissions.contains_key("access_firestore"));
    }
}

//! JSON:API (https://jsonapi.org/format/1.1/) building blocks for /api/v2: resource objects
//! with relationships and links, `include`, pagination, and request documents.
//!
//! Attribute names are kebab-case (`resource-count`, `auto-apply`), the usual JSON:API style.
//! Requests accept the JSON:API document form (`{"data": {"type": ..., "attributes": ...}}`)
//! and, for convenience, the flat object form.

use crate::api::ApiError;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use tf_store::StoreError;

pub const MEDIA: &str = "application/vnd.api+json";
pub const DEFAULT_PAGE_SIZE: usize = 20;
pub const MAX_PAGE_SIZE: usize = 100;

fn invalid(msg: impl Into<String>) -> ApiError {
    StoreError::Invalid(msg.into()).into()
}

/// A JSON response with the JSON:API media type.
pub struct Doc(pub axum::http::StatusCode, pub Value);

impl IntoResponse for Doc {
    fn into_response(self) -> Response {
        let mut r = (self.0, self.1.to_string()).into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(MEDIA));
        r
    }
}

pub fn ok(v: Value) -> Doc {
    Doc(axum::http::StatusCode::OK, v)
}

pub fn created(v: Value) -> Doc {
    Doc(axum::http::StatusCode::CREATED, v)
}

pub fn accepted(v: Value) -> Doc {
    Doc(axum::http::StatusCode::ACCEPTED, v)
}

// ---------- resource objects ----------

fn kebab(key: &str) -> String {
    key.replace('_', "-")
}

fn snake(key: &str) -> String {
    key.replace('-', "_")
}

/// A relationship to one resource, or to none.
pub fn rel(kind: &str, id: Option<&str>) -> Value {
    match id {
        Some(id) => json!({ "data": { "type": kind, "id": id } }),
        None => json!({ "data": null }),
    }
}

/// Builds a resource object from a serialized record. `drop` names fields that are not attributes
/// (the id and foreign keys that appear as relationships). `rels` is (name, resource type, id).
pub fn resource(kind: &str, record: &impl serde::Serialize, drop: &[&str], rels: &[(&str, &str, Option<&str>)], path: &str) -> Value {
    let rec = serde_json::to_value(record).unwrap_or(Value::Null);
    let id = rec.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let mut attrs = Map::new();
    if let Value::Object(m) = rec {
        for (k, v) in m {
            if k == "id" || drop.contains(&k.as_str()) {
                continue;
            }
            attrs.insert(kebab(&k), v);
        }
    }
    let mut relationships = Map::new();
    for (name, rk, rid) in rels {
        relationships.insert((*name).to_string(), rel(rk, *rid));
    }
    let mut out = json!({
        "id": id,
        "type": kind,
        "attributes": attrs,
        "links": { "self": format!("{path}/{id}") },
    });
    if !relationships.is_empty() {
        out["relationships"] = Value::Object(relationships);
    }
    out
}

/// Adds attributes that are computed rather than stored.
pub fn with_attr(mut res: Value, name: &str, value: Value) -> Value {
    res["attributes"][name] = value;
    res
}

pub fn without_attr(mut res: Value, name: &str) -> Value {
    if let Some(m) = res["attributes"].as_object_mut() {
        m.remove(name);
    }
    res
}

/// A single-resource document, with whatever it includes.
pub fn single(res: Value, included: Vec<Value>) -> Value {
    let mut doc = json!({ "data": res });
    if !included.is_empty() {
        doc["included"] = Value::Array(dedup(included));
    }
    doc
}

fn dedup(items: Vec<Value>) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    items.into_iter().filter(|v| seen.insert((v["type"].as_str().unwrap_or("").to_string(), v["id"].as_str().unwrap_or("").to_string()))).collect()
}

// ---------- query: page[...] and include ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub number: usize,
    pub size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub page: Page,
    pub include: Vec<String>,
}

/// Reads `page[number]`, `page[size]` and `include` from a raw query string.
///
/// The size defaults to 20 and is capped at 100 (a larger one is lowered, not refused). A page
/// number or size that is not a positive whole number is refused rather than guessed at, and so
/// is an include that this endpoint does not offer.
pub fn parse_query(raw: Option<&str>, allowed_includes: &[&str]) -> Result<Query, ApiError> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(raw.unwrap_or("")).map_err(|_| invalid("the query string is not valid"))?;
    let (mut number, mut size) = (1usize, DEFAULT_PAGE_SIZE);
    let mut include = Vec::new();
    for (k, v) in pairs {
        match k.as_str() {
            "page[number]" => number = v.parse().ok().filter(|n| *n >= 1).ok_or_else(|| invalid("page[number] must be a whole number of 1 or more"))?,
            "page[size]" => size = v.parse().ok().filter(|n| *n >= 1).ok_or_else(|| invalid("page[size] must be a whole number of 1 or more"))?,
            "include" => {
                for name in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if !allowed_includes.contains(&name) {
                        let offered = if allowed_includes.is_empty() { "none".to_string() } else { allowed_includes.join(", ") };
                        return Err(invalid(format!("unsupported include '{name}'; this endpoint offers: {offered}")));
                    }
                    include.push(name.to_string());
                }
            }
            _ => {}
        }
    }
    Ok(Query { page: Page { number, size: size.min(MAX_PAGE_SIZE) }, include })
}

impl Query {
    pub fn wants(&self, name: &str) -> bool {
        self.include.iter().any(|i| i == name)
    }
}

/// One page of a collection, with `links` and `meta.pagination`. A page past the end is empty,
/// not an error.
pub fn collection(path: &str, q: &Query, all: Vec<Value>, included: Vec<Value>) -> Value {
    let total = all.len();
    let size = q.page.size;
    let pages = total.div_ceil(size).max(1);
    let number = q.page.number;
    let start = (number - 1).saturating_mul(size);
    let data: Vec<Value> = all.into_iter().skip(start).take(size).collect();
    let link = |n: usize| format!("{path}?page%5Bnumber%5D={n}&page%5Bsize%5D={size}");
    let prev = if number > 1 { Some(number - 1) } else { None };
    let next = if number < pages { Some(number + 1) } else { None };
    let mut doc = json!({
        "data": data,
        "links": {
            "self": link(number),
            "first": link(1),
            "prev": prev.map(link),
            "next": next.map(link),
            "last": link(pages),
        },
        "meta": { "pagination": {
            "current-page": number,
            "page-size": size,
            "prev-page": prev,
            "next-page": next,
            "total-pages": pages,
            "total-count": total,
        } },
    });
    if !included.is_empty() {
        doc["included"] = Value::Array(dedup(included));
    }
    doc
}

// ---------- request documents ----------

/// Reads a request body as `T`. In document form, `data.type` must be `kind` (409 otherwise),
/// attributes come from `data.attributes`, and each relationship with an id supplies `<name>_id`.
/// Otherwise the body itself is the object. Attribute names may be kebab-case or snake_case.
pub fn attributes<T: DeserializeOwned>(body: Value, kind: &str) -> Result<T, ApiError> {
    let flat = match body {
        Value::Object(mut m) if m.contains_key("data") => {
            let data = m.remove("data").unwrap_or(Value::Null);
            let Value::Object(mut data) = data else {
                return Err(invalid("\"data\" must be a resource object"));
            };
            if let Some(t) = data.get("type") {
                if t.as_str() != Some(kind) {
                    return Err(StoreError::Conflict(format!("the resource type must be \"{kind}\"")).into());
                }
            }
            let mut out = Map::new();
            match data.remove("attributes") {
                None | Some(Value::Null) => {}
                Some(Value::Object(a)) => {
                    for (k, v) in a {
                        out.insert(snake(&k), v);
                    }
                }
                Some(_) => return Err(invalid("\"attributes\" must be an object")),
            }
            if let Some(Value::Object(rels)) = data.remove("relationships") {
                for (name, r) in rels {
                    if let Some(id) = r.get("data").and_then(|d| d.get("id")).and_then(|i| i.as_str()) {
                        out.insert(format!("{}_id", snake(&name)), Value::String(id.to_string()));
                    }
                }
            }
            Value::Object(out)
        }
        // the plain object form: kebab-case names are accepted here too
        Value::Object(m) => Value::Object(m.into_iter().map(|(k, v)| (snake(&k), v)).collect()),
        other => other,
    };
    serde_json::from_value(flat).map_err(|e| invalid(format!("invalid request body: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, Debug, PartialEq)]
    struct Body {
        name: String,
        #[serde(default)]
        auto_apply: bool,
        #[serde(default)]
        configuration_version_id: Option<String>,
    }

    #[test]
    fn query_defaults_clamps_and_refuses() {
        let q = parse_query(None, &[]).unwrap();
        assert_eq!(q.page, Page { number: 1, size: 20 });
        assert_eq!(parse_query(Some("page%5Bsize%5D=500"), &[]).unwrap().page.size, 100, "capped, not refused");
        assert_eq!(parse_query(Some("page[number]=3&page[size]=5"), &[]).unwrap().page, Page { number: 3, size: 5 });
        for bad in ["page[number]=0", "page[size]=0", "page[size]=x", "page[number]=-1", "page[size]="] {
            assert!(parse_query(Some(bad), &[]).is_err(), "{bad}");
        }
        assert!(parse_query(Some("include=nope"), &["workspace"]).is_err());
        assert_eq!(parse_query(Some("include=workspace"), &["workspace"]).unwrap().include, vec!["workspace"]);
    }

    #[test]
    fn collections_page_with_links_and_meta() {
        let items: Vec<Value> = (0..45).map(|i| json!({ "type": "t", "id": i.to_string() })).collect();
        let q = |n, s| Query { page: Page { number: n, size: s }, include: vec![] };
        let d = collection("/api/v2/x", &q(1, 20), items.clone(), vec![]);
        assert_eq!(d["data"].as_array().unwrap().len(), 20);
        assert_eq!(d["meta"]["pagination"], json!({ "current-page": 1, "page-size": 20, "prev-page": null, "next-page": 2, "total-pages": 3, "total-count": 45 }));
        assert!(d["links"]["prev"].is_null() && !d["links"]["next"].is_null());
        let d = collection("/api/v2/x", &q(3, 20), items.clone(), vec![]);
        assert_eq!(d["data"].as_array().unwrap().len(), 5);
        assert!(d["links"]["next"].is_null());
        let d = collection("/api/v2/x", &q(9, 20), items, vec![]);
        assert!(d["data"].as_array().unwrap().is_empty(), "past the end is empty");
        assert_eq!(d["meta"]["pagination"]["total-count"], 45);
        let d = collection("/api/v2/x", &q(1, 20), vec![], vec![]);
        assert_eq!((d["meta"]["pagination"]["total-pages"].clone(), d["meta"]["pagination"]["total-count"].clone()), (json!(1), json!(0)), "an empty list is one empty page");
    }

    #[test]
    fn resources_are_kebab_cased_and_keep_foreign_keys_as_relationships() {
        #[derive(serde::Serialize)]
        struct R {
            id: String,
            workspace_id: String,
            auto_apply: bool,
        }
        let r = resource("runs", &R { id: "r1".into(), workspace_id: "w1".into(), auto_apply: true }, &["workspace_id"], &[("workspace", "workspaces", Some("w1"))], "/api/v2/runs");
        assert_eq!(r["id"], "r1");
        assert_eq!(r["type"], "runs");
        assert_eq!(r["attributes"], json!({ "auto-apply": true }));
        assert_eq!(r["relationships"]["workspace"]["data"], json!({ "type": "workspaces", "id": "w1" }));
        assert_eq!(r["links"]["self"], "/api/v2/runs/r1");
    }

    #[test]
    fn request_documents_and_flat_objects_both_work() {
        let doc = json!({ "data": { "type": "runs", "attributes": { "name": "n", "auto-apply": true },
            "relationships": { "configuration-version": { "data": { "type": "configuration-versions", "id": "cv1" } } } } });
        let b: Body = attributes(doc, "runs").unwrap();
        assert_eq!(b, Body { name: "n".into(), auto_apply: true, configuration_version_id: Some("cv1".into()) });
        let flat: Body = attributes(json!({ "name": "n", "auto-apply": true }), "runs").unwrap();
        assert_eq!((flat.name.as_str(), flat.auto_apply), ("n", true), "kebab-case names work in the plain form too");
        assert!(matches!(attributes::<Body>(json!({ "data": { "type": "workspaces", "attributes": { "name": "n" } } }), "runs"), Err(ApiError(StoreError::Conflict(_)))), "wrong type is a conflict");
        assert!(attributes::<Body>(json!({ "data": [] }), "runs").is_err());
        assert!(attributes::<Body>(json!({ "data": { "type": "runs", "attributes": [] } }), "runs").is_err());
        assert!(attributes::<Body>(json!({ "data": { "type": "runs", "attributes": {} } }), "runs").is_err(), "missing name");
    }
}

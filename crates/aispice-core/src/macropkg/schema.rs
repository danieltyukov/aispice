//! The macro/v1 JSON schema and a small validator for the part of JSON
//! Schema it uses.
//!
//! A JSON Schema library would word its errors its own way, and ChipLeap
//! validates the same documents in Python with identical messages. So both
//! tools carry this interpreter for the keywords the schema uses, applied in
//! one fixed order: `$ref`, `type` (stop on failure), `const`, `enum`,
//! `minLength`, `maxLength`, `pattern`, `minimum`, `exclusiveMinimum`,
//! `required`, `properties` (by sorted key), `additionalProperties` (by sorted
//! key), `minProperties`, `minItems`, `items`, then `if`/`then`/`else`. Other
//! keywords are annotations.

use super::text::{describe, fmt_g, join, plural, quote};
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// `macro-v1.schema.json`, identical to ChipLeap's copy.
pub const SCHEMA_JSON: &str = include_str!("macro-v1.schema.json");

fn schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| serde_json::from_str(SCHEMA_JSON).expect("the bundled schema is JSON"))
}

/// A parsed TOML or JSON document. TOML dates and times stay distinct, so
/// they fail every type check, as they do in Python.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Val>),
    Obj(BTreeMap<String, Val>),
    Datetime,
}

impl From<&toml::Value> for Val {
    fn from(v: &toml::Value) -> Self {
        match v {
            toml::Value::String(s) => Val::Str(s.clone()),
            toml::Value::Integer(i) => Val::Int(*i),
            toml::Value::Float(f) => Val::Float(*f),
            toml::Value::Boolean(b) => Val::Bool(*b),
            toml::Value::Datetime(_) => Val::Datetime,
            toml::Value::Array(a) => Val::Arr(a.iter().map(Val::from).collect()),
            toml::Value::Table(t) => {
                Val::Obj(t.iter().map(|(k, v)| (k.clone(), Val::from(v))).collect())
            }
        }
    }
}

impl From<&Value> for Val {
    fn from(v: &Value) -> Self {
        match v {
            Value::Null => Val::Null,
            Value::Bool(b) => Val::Bool(*b),
            Value::Number(n) => match n.as_i64() {
                Some(i) => Val::Int(i),
                None => Val::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            Value::String(s) => Val::Str(s.clone()),
            Value::Array(a) => Val::Arr(a.iter().map(Val::from).collect()),
            Value::Object(o) => {
                Val::Obj(o.iter().map(|(k, v)| (k.clone(), Val::from(v))).collect())
            }
        }
    }
}

impl Val {
    pub fn get(&self, key: &str) -> Option<&Val> {
        match self {
            Val::Obj(o) => o.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Int(i) => Some(*i as f64),
            Val::Float(f) => Some(*f),
            _ => None,
        }
    }
}

fn type_name(kind: &str) -> &'static str {
    match kind {
        "object" => "a table",
        "array" => "an array",
        "string" => "a string",
        "number" => "a number",
        "integer" => "an integer",
        "boolean" => "true or false",
        _ => "of another type",
    }
}

fn type_ok(kind: &str, v: &Val) -> bool {
    match kind {
        "object" => matches!(v, Val::Obj(_)),
        "array" => matches!(v, Val::Arr(_)),
        "string" => matches!(v, Val::Str(_)),
        "boolean" => matches!(v, Val::Bool(_)),
        "number" => matches!(v, Val::Int(_) | Val::Float(_)),
        "integer" => match v {
            Val::Int(_) => true,
            Val::Float(f) => f.is_finite() && f.fract() == 0.0,
            _ => false,
        },
        _ => false,
    }
}

/// Equality of a document value with a schema constant, as Python's `==`
/// sees it for the scalars the schema uses.
fn equals(v: &Val, c: &Value) -> bool {
    match (v, c) {
        (Val::Null, Value::Null) => true,
        (Val::Bool(a), Value::Bool(b)) => a == b,
        (Val::Str(a), Value::String(b)) => a == b,
        (Val::Int(_) | Val::Float(_), Value::Number(n)) => v.as_f64() == n.as_f64(),
        _ => false,
    }
}

fn label(path: &str) -> &str {
    if path.is_empty() {
        "the document"
    } else {
        path
    }
}

fn resolve(r: &str) -> Option<&'static Value> {
    let name = r.strip_prefix("#/$defs/")?;
    schema().get("$defs")?.get(name)
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn count(v: &Value) -> usize {
    v.as_u64().unwrap_or(0) as usize
}

/// Problems with `value` against schema `node`, as messages, in a fixed order.
pub fn validate(node: &Value, value: &Val, path: &str) -> Vec<String> {
    let mut errs = Vec::new();
    let obj = match node {
        Value::Bool(true) => return errs,
        Value::Bool(false) => return vec![format!("{} is not allowed here", label(path))],
        Value::Object(o) => o,
        _ => return errs,
    };
    if let Some(r) = obj.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve(r)
    {
        errs.extend(validate(target, value, path));
    }
    if let Some(kind) = obj.get("type").and_then(Value::as_str)
        && !type_ok(kind, value)
    {
        errs.push(format!("{} must be {}", label(path), type_name(kind)));
        return errs;
    }
    if let Some(c) = obj.get("const")
        && !equals(value, c)
    {
        errs.push(format!(
            "{} must be {}",
            label(path),
            describe(&Val::from(c))
        ));
    }
    if let Some(Value::Array(options)) = obj.get("enum")
        && !options.iter().any(|o| equals(value, o))
    {
        let allowed: Vec<String> = options
            .iter()
            .map(|o| {
                o.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| o.to_string())
            })
            .collect();
        errs.push(format!(
            "{} is {}, expected one of {}",
            label(path),
            describe(value),
            allowed.join(", ")
        ));
    }
    if let Val::Str(s) = value {
        let len = s.chars().count();
        if let Some(n) = obj.get("minLength").map(count)
            && len < n
        {
            errs.push(format!(
                "{} must have at least {}",
                label(path),
                plural(n, "character", None)
            ));
        }
        if let Some(n) = obj.get("maxLength").map(count)
            && len > n
        {
            errs.push(format!(
                "{} must have at most {}",
                label(path),
                plural(n, "character", None)
            ));
        }
        if let Some(p) = obj.get("pattern").and_then(Value::as_str)
            && !Regex::new(p).is_ok_and(|re| re.is_match(s))
        {
            errs.push(format!(
                "{} is {}, which does not match {p}",
                label(path),
                quote(s)
            ));
        }
    }
    if let Some(x) = value.as_f64() {
        if let Some(m) = obj.get("minimum")
            && x < num(m)
        {
            errs.push(format!(
                "{} must be at least {}",
                label(path),
                fmt_g(num(m))
            ));
        }
        if let Some(m) = obj.get("exclusiveMinimum")
            && x <= num(m)
        {
            errs.push(format!(
                "{} must be greater than {}",
                label(path),
                fmt_g(num(m))
            ));
        }
    }
    if let Val::Obj(map) = value {
        if let Some(Value::Array(req)) = obj.get("required") {
            for key in req.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    errs.push(format!("{} is missing", join(path, key)));
                }
            }
        }
        let empty = serde_json::Map::new();
        let props = obj
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let mut keys: Vec<&String> = props.keys().collect();
        keys.sort();
        for key in keys {
            if let Some(v) = map.get(key.as_str()) {
                errs.extend(validate(&props[key.as_str()], v, &join(path, key)));
            }
        }
        let extra = obj
            .get("additionalProperties")
            .unwrap_or(&Value::Bool(true));
        // BTreeMap iterates in sorted key order, as Python's sorted() does.
        for (key, v) in map {
            if props.contains_key(key.as_str()) {
                continue;
            }
            match extra {
                Value::Bool(true) => {}
                Value::Bool(false) => {
                    errs.push(format!("{} is not a known field", join(path, key)))
                }
                other => errs.extend(validate(other, v, &join(path, key))),
            }
        }
        if let Some(n) = obj.get("minProperties").map(count)
            && map.len() < n
        {
            errs.push(format!(
                "{} must have at least {}",
                label(path),
                plural(n, "entry", Some("entries"))
            ));
        }
    }
    if let Val::Arr(items) = value {
        if let Some(n) = obj.get("minItems").map(count)
            && items.len() < n
        {
            errs.push(format!(
                "{} must have at least {}",
                label(path),
                plural(n, "item", None)
            ));
        }
        if let Some(item_schema) = obj.get("items") {
            for (i, item) in items.iter().enumerate() {
                errs.extend(validate(item_schema, item, &format!("{path}[{i}]")));
            }
        }
    }
    if let Some(cond) = obj.get("if") {
        let branch = if validate(cond, value, path).is_empty() {
            "then"
        } else {
            "else"
        };
        if let Some(b) = obj.get(branch) {
            errs.extend(validate(b, value, path));
        }
    }
    errs
}

/// Validate against `#/$defs/<name>`, or the root schema when `name` is empty.
pub fn validate_def(name: &str, value: &Val) -> Vec<String> {
    let node = if name.is_empty() {
        Some(schema())
    } else {
        schema().get("$defs").and_then(|d| d.get(name))
    };
    match node {
        Some(n) => validate(n, value, ""),
        None => vec![format!("the schema has no definition {name}")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Val {
        let text = include_str!("../../tests/fixtures/macro/tiny_counter.macro/macro.toml");
        Val::from(&toml::Value::Table(toml::from_str(text).unwrap()))
    }

    fn set(v: &mut Val, path: &[&str], new: Option<Val>) {
        let mut cur = v;
        for (k, key) in path.iter().enumerate() {
            let last = k == path.len() - 1;
            cur = match cur {
                Val::Obj(o) if last => {
                    match new {
                        Some(n) => o.insert(key.to_string(), n),
                        None => o.remove(*key),
                    };
                    return;
                }
                Val::Obj(o) => o.get_mut(*key).unwrap(),
                Val::Arr(a) => {
                    let i: usize = key.parse().unwrap();
                    if last {
                        a[i] = new.unwrap();
                        return;
                    }
                    &mut a[i]
                }
                _ => panic!("bad path"),
            };
        }
    }

    #[test]
    fn messages_match_chipleap() {
        // The same cases as ChipLeap's test_schema_messages.
        let missing: Vec<String> = [
            "schema", "name", "kind", "pdk", "producer", "size", "domains", "pins",
        ]
        .iter()
        .map(|k| format!("{k} is missing"))
        .collect();
        assert_eq!(validate_def("", &Val::Obj(BTreeMap::new())), missing);
        let mut doc = manifest();
        set(
            &mut doc,
            &["pins", "4", "domain"],
            Some(Val::Str("core".into())),
        );
        set(&mut doc, &["pins", "3", "width"], Some(Val::Int(0)));
        set(
            &mut doc,
            &["size", "width_um"],
            Some(Val::Str("wide".into())),
        );
        set(&mut doc, &["colour"], Some(Val::Int(1)));
        assert_eq!(
            validate_def("", &doc),
            vec![
                "pins[3].width must be at least 1",
                "pins[4].domain is not allowed here",
                "size.width_um must be a number",
                "colour is not a known field",
            ]
        );
        assert_eq!(
            validate_def("evidence", &Val::Arr(vec![])),
            vec!["the document must be a table"]
        );
    }

    #[test]
    fn patterns_are_anchored_at_the_very_end() {
        let mut doc = manifest();
        set(&mut doc, &["name"], Some(Val::Str("abc\n".into())));
        assert_eq!(
            validate_def("", &doc),
            vec!["name is \"abc\\n\", which does not match ^[A-Za-z_][A-Za-z0-9_]*$"]
        );
    }

    #[test]
    fn integers_and_dates() {
        let mut doc = manifest();
        set(&mut doc, &["pins", "3", "width"], Some(Val::Float(2.0)));
        assert!(validate_def("", &doc).is_empty());
        set(&mut doc, &["pins", "3", "width"], Some(Val::Float(2.5)));
        assert_eq!(
            validate_def("", &doc),
            vec!["pins[3].width must be an integer"]
        );
        set(&mut doc, &["pins", "3", "width"], Some(Val::Bool(true)));
        assert_eq!(
            validate_def("", &doc),
            vec!["pins[3].width must be an integer"]
        );
        set(&mut doc, &["pins", "3", "width"], Some(Val::Int(2)));
        set(&mut doc, &["producer", "version"], Some(Val::Datetime));
        assert_eq!(
            validate_def("", &doc),
            vec!["producer.version must be a string"]
        );
    }
}

/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Plans: the registry written down as JSON, read back out of a server
//! (`snapshot`) and made true on one (`apply`).
//!
//! A plan is one JSON object keyed by registry type. A singleton's value is
//! an object of its properties; a collection's is an array of objects:
//!
//! ```json
//! {
//!   "x:Domain": [ { "name": "example.com", "isEnabled": true } ],
//!   "x:Account": [ { "@type": "User", "name": "alice",
//!                    "domainId": "@x:Domain/example.com" } ],
//!   "x:Http": { "usePermissiveCors": false }
//! }
//! ```
//!
//! An object is matched to one on the server by its key: the property that
//! names it (a domain's `name`), or `@key` where that property is the
//! server's to set (an account's address). A type with no such property, like
//! tracers, is matched by `@id`, which ties the plan to one server.
//!
//! Ids differ from server to server, so a property that refers to another
//! object holds `@<type>/<key>` instead of an id, and `apply` looks it up.
//! Only the properties a plan names are managed; the rest are left as they
//! are. Secrets are never read back, so a snapshot has none.

use std::collections::{HashMap, HashSet};
use std::io::Write;

use serde_json::{Map, Value, json};

use crate::client::{Client, set_error};
use crate::error::{Error, Result};
use crate::reload::Tracker;
use crate::schema::{Kind, Reference, Schema, Update};

/// What `apply` needs from a server, so it can be tested without one.
pub trait Registry {
    fn get_all(&self, object: &str) -> Result<Vec<Map<String, Value>>>;
    fn get_singleton(&self, object: &str) -> Result<Option<Map<String, Value>>>;
    fn set(&self, object: &str, args: Value) -> Result<Map<String, Value>>;
    fn max_objects_in_set(&self) -> usize;
}

impl Registry for Client {
    fn get_all(&self, object: &str) -> Result<Vec<Map<String, Value>>> {
        Client::get_all(self, object)
    }

    fn get_singleton(&self, object: &str) -> Result<Option<Map<String, Value>>> {
        Ok(self.get(object, &["singleton".to_owned()])?.into_iter().next())
    }

    fn set(&self, object: &str, args: Value) -> Result<Map<String, Value>> {
        match self.call(&format!("{object}/set"), args)? {
            Value::Object(map) => Ok(map),
            _ => Err(Error::Transport(format!("{object}/set answered something odd"))),
        }
    }

    fn max_objects_in_set(&self) -> usize {
        self.max_objects_in_set
    }
}

/// Types a snapshot leaves out unless asked for by name: records, telemetry
/// and operations rather than settings, per-person credentials, and DKIM keys,
/// which the server makes for each domain.
pub const NOT_SETTINGS: &[&str] = &[
    "x:AccountPassword",
    "x:Action",
    "x:ApiKey",
    "x:AppPassword",
    "x:ArchivedItem",
    "x:ArfExternalReport",
    "x:Bootstrap",
    "x:ClusterNode",
    "x:DkimSignature",
    "x:DmarcExternalReport",
    "x:DmarcInternalReport",
    "x:Log",
    "x:MaskedEmail",
    "x:Metric",
    "x:PublicKey",
    "x:QueuedMessage",
    "x:SpamTrainingSample",
    "x:Task",
    "x:TlsExternalReport",
    "x:TlsInternalReport",
    "x:Trace",
];

/// A reference as a plan writes it: `@x:Domain/example.com`.
pub fn format_ref(object: &str, key: &str) -> String {
    format!("@{object}/{key}")
}

/// The type and key of a reference, if `value` is one.
pub fn parse_ref(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix('@')?;
    if !rest.starts_with("x:") {
        return None;
    }
    let (object, key) = rest.split_once('/')?;
    (!key.is_empty()).then_some((object, key))
}

/// A key as a string, whatever JSON type the property is.
fn key_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The key of an object on the server, or None if it has none. Where keys
/// repeat across namespaces, the namespace comes first:
/// `trusted-domains/example.com`.
fn server_key(schema: &Schema, object: &str, item: &Map<String, Value>) -> Option<String> {
    let key = item.get(&schema.key_property(object)?).and_then(key_text)?;
    match schema
        .key_qualifier(object)
        .and_then(|q| item.get(&q).and_then(key_text))
    {
        Some(scope) => Some(format!("{scope}/{key}")),
        None => Some(key),
    }
}

/// The key of an object in a plan: `@key`, else its key property.
fn plan_key(schema: &Schema, object: &str, item: &Map<String, Value>) -> Option<String> {
    item.get("@key")
        .and_then(key_text)
        .or_else(|| server_key(schema, object, item))
}

/// Ids and keys of the objects of each type, looked up as they're needed.
/// An id of None is an object a dry run would have created.
struct Index<'a, R: Registry> {
    registry: &'a R,
    schema: &'a Schema,
    types: HashMap<String, TypeIndex>,
}

#[derive(Default)]
struct TypeIndex {
    by_key: HashMap<String, Option<String>>,
    by_id: HashMap<String, String>,
}

impl<'a, R: Registry> Index<'a, R> {
    fn new(registry: &'a R, schema: &'a Schema) -> Self {
        Index {
            registry,
            schema,
            types: HashMap::new(),
        }
    }

    fn load(&mut self, object: &str) -> Result<&mut TypeIndex> {
        if !self.types.contains_key(object) {
            let mut index = TypeIndex::default();
            if self.schema.kind(object) == Kind::Object {
                for item in self.registry.get_all(object)? {
                    if let (Some(id), Some(key)) = (
                        item.get("id").and_then(Value::as_str),
                        server_key(self.schema, object, &item),
                    ) {
                        index.by_key.insert(key.clone(), Some(id.to_owned()));
                        index.by_id.insert(id.to_owned(), key);
                    }
                }
            }
            self.types.insert(object.to_owned(), index);
        }
        Ok(self.types.get_mut(object).expect("just inserted"))
    }

    /// Seeds a type's index from objects already fetched.
    fn seed(&mut self, object: &str, items: &[Map<String, Value>]) {
        let mut index = TypeIndex::default();
        for item in items {
            if let (Some(id), Some(key)) = (
                item.get("id").and_then(Value::as_str),
                server_key(self.schema, object, item),
            ) {
                index.by_key.insert(key.clone(), Some(id.to_owned()));
                index.by_id.insert(id.to_owned(), key);
            }
        }
        self.types.insert(object.to_owned(), index);
    }

    fn learn(&mut self, object: &str, key: &str, id: Option<&str>) {
        let index = self.types.entry(object.to_owned()).or_default();
        index.by_key.insert(key.to_owned(), id.map(str::to_owned));
        if let Some(id) = id {
            index.by_id.insert(id.to_owned(), key.to_owned());
        }
    }

    fn key_of(&mut self, object: &str, id: &str) -> Result<Option<String>> {
        Ok(self.load(object)?.by_id.get(id).cloned())
    }

    /// `Some(Some(id))` for an object that exists, `Some(None)` for one a dry
    /// run would have created, None for one that doesn't exist.
    fn id_of(&mut self, object: &str, key: &str) -> Result<Option<Option<String>>> {
        Ok(self.load(object)?.by_key.get(key).cloned())
    }
}

/// The ids in a reference property, whether written as one id, an array, or
/// a set (`{"id": true}`).
fn ids_in(value: &Value) -> Vec<&str> {
    match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        Value::Object(o) => o.keys().map(String::as_str).collect(),
        _ => Vec::new(),
    }
}

/// `value` with each id replaced by `f(id)`, in the same shape.
fn map_ids(value: &Value, mut f: impl FnMut(&str) -> String) -> Value {
    match value {
        Value::String(s) => Value::String(f(s)),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|v| v.as_str().map_or(v.clone(), |s| Value::String(f(s))))
                .collect(),
        ),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (f(k), v.clone())).collect()),
        other => other.clone(),
    }
}

/// Writes a server object's references as `@type/key`, where the target has
/// a key. Returns the targets it couldn't name.
fn refs_to_keys<R: Registry>(
    index: &mut Index<R>,
    refs: &[Reference],
    item: &mut Map<String, Value>,
) -> Result<Vec<String>> {
    let mut unnamed = Vec::new();
    for r in refs {
        let Some(value) = item.get(&r.property) else {
            continue;
        };
        if index.schema.key_property(&r.target).is_none() {
            continue;
        }
        let mut keys = HashMap::new();
        for id in ids_in(value) {
            match index.key_of(&r.target, id)? {
                Some(key) => {
                    keys.insert(id.to_owned(), format_ref(&r.target, &key));
                }
                None => unnamed.push(format!("{} {id}", r.target)),
            }
        }
        let mapped = map_ids(value, |id| keys.get(id).cloned().unwrap_or_else(|| id.to_owned()));
        item.insert(r.property.clone(), mapped);
    }
    Ok(unnamed)
}

/// Placeholder for an id a dry run doesn't have yet.
fn placeholder(object: &str, key: &str) -> String {
    format!("(new {object} {key})")
}

/// Replaces `@type/key` references in a plan object with ids. Returns the
/// references that name nothing, yet.
fn keys_to_refs<R: Registry>(
    index: &mut Index<R>,
    item: &mut Map<String, Value>,
) -> Result<Vec<(String, String)>> {
    let mut missing = Vec::new();
    let names: Vec<String> = item.keys().cloned().collect();
    for name in names {
        if name.starts_with('@') {
            continue;
        }
        let value = item[&name].clone();
        let mut resolved: HashMap<String, String> = HashMap::new();
        for text in ids_in(&value) {
            if let Some((object, key)) = parse_ref(text) {
                match index.id_of(object, key)? {
                    Some(Some(id)) => {
                        resolved.insert(text.to_owned(), id);
                    }
                    Some(None) => {
                        resolved.insert(text.to_owned(), placeholder(object, key));
                    }
                    None => missing.push((object.to_owned(), key.to_owned())),
                }
            }
        }
        if !resolved.is_empty() {
            item.insert(
                name,
                map_ids(&value, |t| {
                    resolved.get(t).cloned().unwrap_or_else(|| t.to_owned())
                }),
            );
        }
    }
    Ok(missing)
}

/// Replaces `@type/key` references in `item` with ids, for a create or update
/// given on the command line.
pub fn resolve_refs<R: Registry>(
    registry: &R,
    schema: &Schema,
    object: &str,
    item: &mut Map<String, Value>,
) -> Result<()> {
    let mut index = Index::new(registry, schema);
    let missing = keys_to_refs(&mut index, item)?;
    sets_as_maps(schema, object, item);
    match missing.first() {
        Some((t, k)) => Err(Error::Usage(format!("{} doesn't exist", format_ref(t, k)))),
        None => Ok(()),
    }
}

/// The id of the object `text` names: an id, its key, or `@type/key`.
pub fn find_id<R: Registry>(registry: &R, schema: &Schema, object: &str, text: &str) -> Result<String> {
    let key = match parse_ref(text) {
        Some((t, k)) if t == object => k,
        Some((t, _)) => return Err(Error::Usage(format!("{text} is a {t}, not a {object}"))),
        None => text,
    };
    let items = registry.get_all(object)?;
    if items
        .iter()
        .any(|i| i.get("id").and_then(Value::as_str) == Some(text))
    {
        return Ok(text.to_owned());
    }
    items
        .iter()
        .find(|i| server_key(schema, object, i).as_deref() == Some(key))
        .and_then(|i| i.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| Error::Usage(format!("no {object} {text}")))
}

/// One server object as a plan writes it: no id, nothing the server sets, no
/// secrets, and references by key.
fn to_plan_object<R: Registry>(
    index: &mut Index<R>,
    object: &str,
    mut item: Map<String, Value>,
    warnings: &mut Vec<String>,
) -> Result<Map<String, Value>> {
    let schema = index.schema;
    let key_property = schema.key_property(object);
    let refs = schema.references(object, Some(&item));
    let unnamed = refs_to_keys(index, &refs, &mut item)?;
    for u in unnamed {
        warnings.push(format!(
            "{object}: refers to {u}, which has no key, so its id stays"
        ));
    }

    let props = schema
        .properties(object, Some(&item))
        .cloned()
        .unwrap_or_default();
    let mut out = Map::new();
    if let Some(t) = item.get("@type") {
        out.insert("@type".into(), t.clone());
    }
    let mut keyed = false;
    for (name, value) in &item {
        if name == "id" || name == "@type" || value.is_null() {
            continue;
        }
        if let Some(spec) = props.get(name) {
            if schema.holds_secret(spec) {
                continue;
            }
            if Schema::update_rule(spec) == Update::ServerSet {
                if key_property.as_deref() == Some(name.as_str())
                    && let Some(k) = key_text(value)
                {
                    out.insert("@key".into(), Value::String(k));
                    keyed = true;
                }
                continue;
            }
        }
        if key_property.as_deref() == Some(name.as_str()) && key_text(value).is_some() {
            keyed = true;
        }
        out.insert(name.clone(), value.clone());
    }
    if !keyed
        && schema.kind(object) == Kind::Object
        && let Some(id) = item.get("id")
    {
        out.insert("@id".into(), id.clone());
    }
    Ok(out)
}

/// The registry as a plan, for the types asked for.
pub fn snapshot<R: Registry>(
    registry: &R,
    schema: &Schema,
    types: &[String],
) -> Result<(Value, Vec<String>)> {
    let mut index = Index::new(registry, schema);
    let mut warnings = Vec::new();
    let mut plan = Map::new();
    for object in types {
        match schema.kind(object) {
            Kind::Singleton => {
                let Some(item) = registry.get_singleton(object)? else {
                    warnings.push(format!("{object}: the server returned nothing"));
                    continue;
                };
                let out = to_plan_object(&mut index, object, item, &mut warnings)?;
                plan.insert(object.clone(), Value::Object(out));
            }
            Kind::Object => {
                let items = registry.get_all(object)?;
                index.seed(object, &items);
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(to_plan_object(&mut index, object, item, &mut warnings)?);
                }
                out.sort_by_key(|o| {
                    plan_key(schema, object, o)
                        .or_else(|| o.get("@id").and_then(key_text))
                        .unwrap_or_default()
                });
                plan.insert(
                    object.clone(),
                    Value::Array(out.into_iter().map(Value::Object).collect()),
                );
            }
        }
    }
    Ok((Value::Object(plan), warnings))
}

/// A plan file's types, resolved against the schema, each with its value.
pub fn read_plan(schema: &Schema, plan: Value) -> Result<Vec<(String, Value)>> {
    let Value::Object(map) = plan else {
        return Err(Error::Usage("a plan is a JSON object keyed by type".into()));
    };
    let mut out: Vec<(String, Value)> = Vec::new();
    for (name, value) in map {
        let object = schema.resolve(&name)?;
        if out.iter().any(|(o, _)| *o == object) {
            return Err(Error::Usage(format!("the plan names {object} twice")));
        }
        match (schema.kind(&object), &value) {
            (Kind::Singleton, Value::Object(_)) | (Kind::Object, Value::Array(_)) => {}
            (Kind::Singleton, _) => {
                return Err(Error::Usage(format!(
                    "{object} is a singleton: give it an object"
                )));
            }
            (Kind::Object, _) => {
                return Err(Error::Usage(format!(
                    "{object} is a collection: give it an array"
                )));
            }
        }
        if let Value::Array(items) = &value
            && let Some(bad) = items.iter().position(|i| !i.is_object())
        {
            return Err(Error::Usage(format!("{object}[{bad}] isn't an object")));
        }
        out.push((object, value));
    }
    Ok(out)
}

/// The plan's types, each after the types it refers to. A cycle keeps the
/// plan's own order for what's left.
fn ordered(schema: &Schema, plan: &[(String, Value)]) -> Vec<usize> {
    let names: Vec<&str> = plan.iter().map(|(n, _)| n.as_str()).collect();
    let mut done: Vec<usize> = Vec::new();
    let mut left: Vec<usize> = (0..plan.len()).collect();
    while !left.is_empty() {
        let ready = left.iter().position(|&i| {
            schema
                .referenced_types(names[i])
                .iter()
                .all(|t| t == names[i] || !names.contains(&t.as_str()) || done.iter().any(|&d| names[d] == t))
        });
        let pick = left.remove(ready.unwrap_or(0));
        done.push(pick);
    }
    done
}

#[derive(Default, Debug)]
pub struct Options {
    pub dry_run: bool,
    pub prune: bool,
    pub write_secrets: bool,
}

#[derive(Default, Debug)]
pub struct Outcome {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub destroyed: usize,
    pub failed: usize,
}

/// JSON Pointer escaping for a patch path segment (RFC 6901).
fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn numbers_equal(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// Whether two values hold the same set of ids or members, ignoring order and
/// whether the set is written as an array or as `{"x": true}`.
fn same_set(a: &Value, b: &Value) -> bool {
    let set = |v: &Value| -> Option<HashSet<String>> {
        match v {
            Value::Array(items) => Some(items.iter().map(|i| i.to_string()).collect()),
            Value::Object(o) => Some(o.keys().map(|k| Value::String(k.clone()).to_string()).collect()),
            Value::Null => Some(HashSet::new()),
            _ => None,
        }
    };
    match (set(a), set(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// Patch entries that make `current` agree with `desired` under `path`. A
/// nested object is compared property by property, so what the plan leaves
/// out keeps its value; anything else is replaced whole.
fn diff_value(path: &str, desired: &Value, current: &Value, patch: &mut Map<String, Value>) {
    match (desired, current) {
        (Value::Object(d), Value::Object(c)) if d.get("@type") == c.get("@type") => {
            for (k, v) in d {
                if k == "@type" {
                    continue;
                }
                diff_value(
                    &format!("{path}/{}", escape(k)),
                    v,
                    c.get(k).unwrap_or(&Value::Null),
                    patch,
                );
            }
        }
        _ if numbers_equal(desired, current) => {}
        _ => {
            patch.insert(path.to_owned(), desired.clone());
        }
    }
}

/// The update that makes `current` what `desired` says, or why there can't be
/// one.
fn diff(
    schema: &Schema,
    object: &str,
    desired: &Map<String, Value>,
    current: &Map<String, Value>,
    options: &Options,
) -> std::result::Result<Map<String, Value>, String> {
    if let (Some(d), Some(c)) = (desired.get("@type"), current.get("@type"))
        && d != c
    {
        return Err(format!(
            "is a {c} on the server and a {d} in the plan: delete it and apply again"
        ));
    }
    let props = schema
        .properties(object, Some(current))
        .cloned()
        .unwrap_or_default();
    let mut patch = Map::new();
    for (name, value) in desired {
        if name.starts_with('@') {
            continue;
        }
        let spec = props.get(name);
        if let Some(spec) = spec {
            if Schema::update_rule(spec) == Update::ServerSet {
                continue;
            }
            if schema.holds_secret(spec) && !options.write_secrets {
                continue;
            }
        }
        let now = current.get(name).unwrap_or(&Value::Null);
        let kind = spec
            .and_then(|s| s.get("type"))
            .and_then(|t| t.get("type"))
            .and_then(Value::as_str);
        let mut entries = Map::new();
        match kind {
            Some("set" | "map" | "list" | "objectList") => {
                if !same_set(value, now) && value != now {
                    entries.insert(escape(name), value.clone());
                }
            }
            _ => diff_value(&escape(name), value, now, &mut entries),
        }
        if entries.is_empty() {
            continue;
        }
        if spec.is_some_and(|s| Schema::update_rule(s) == Update::Immutable) {
            return Err(format!(
                "{name} can't be changed once set: delete it and apply again"
            ));
        }
        patch.extend(entries);
    }
    Ok(patch)
}

/// A set written as an array in a plan, as JMAP writes sets: `{"a": true}`.
fn sets_as_maps(schema: &Schema, object: &str, item: &mut Map<String, Value>) {
    let Some(props) = schema.properties(object, Some(item)).cloned() else {
        return;
    };
    for (name, spec) in props {
        let is_set = spec
            .get("type")
            .and_then(|t| t.get("type"))
            .and_then(Value::as_str)
            == Some("set");
        if let Some(Value::Array(members)) = item.get(&name)
            && is_set
        {
            let map: Map<String, Value> = members
                .iter()
                .filter_map(|m| Some((key_text(m)?, Value::Bool(true))))
                .collect();
            item.insert(name, Value::Object(map));
        }
    }
}

/// The object as it is created: plan markers dropped.
fn creatable(item: &Map<String, Value>) -> Map<String, Value> {
    item.iter()
        .filter(|(k, _)| !k.starts_with('@') || *k == "@type")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// A collection the plan named: its type, the ids the plan matched, and what
/// the server had.
type Matched = (String, HashSet<String>, Vec<Map<String, Value>>);

pub struct Applier<'a, R: Registry> {
    index: Index<'a, R>,
    registry: &'a R,
    schema: &'a Schema,
    options: Options,
    pub outcome: Outcome,
    pub tracker: Tracker,
    /// Ids each collection's plan matched, for pruning the rest.
    matched: Vec<Matched>,
}

impl<'a, R: Registry> Applier<'a, R> {
    pub fn new(registry: &'a R, schema: &'a Schema, options: Options) -> Self {
        Applier {
            index: Index::new(registry, schema),
            registry,
            schema,
            options,
            outcome: Outcome::default(),
            tracker: Tracker::default(),
            matched: Vec::new(),
        }
    }

    // What apply does goes to standard output a line at a time; a reader
    // that has gone away doesn't stop the run.
    fn say(&self, line: String) {
        let _ = writeln!(std::io::stdout(), "{line}");
    }

    fn fail(&mut self, line: String) {
        self.outcome.failed += 1;
        let _ = writeln!(std::io::stdout(), "! {line}");
    }

    /// Sends one /set, unless this is a dry run, and notes what it changed.
    fn write(&mut self, object: &str, args: Value) -> Result<Map<String, Value>> {
        let answer = self.registry.set(object, args)?;
        let changed = ["created", "updated"].iter().any(|k| {
            answer
                .get(*k)
                .and_then(Value::as_object)
                .is_some_and(|m| !m.is_empty())
        }) || answer
            .get("destroyed")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        if changed {
            self.tracker.record(object, &answer);
        }
        Ok(answer)
    }

    pub fn run(&mut self, plan: &[(String, Value)]) -> Result<()> {
        for i in ordered(self.schema, plan) {
            let (object, value) = &plan[i];
            match value {
                Value::Object(item) => self.singleton(object, item)?,
                Value::Array(items) => {
                    let items: Vec<Map<String, Value>> =
                        items.iter().filter_map(|v| v.as_object().cloned()).collect();
                    self.collection(object, items)?;
                }
                _ => {}
            }
        }
        if self.options.prune {
            self.prune()?;
        }
        Ok(())
    }

    fn singleton(&mut self, object: &str, item: &Map<String, Value>) -> Result<()> {
        let mut desired = item.clone();
        let missing = keys_to_refs(&mut self.index, &mut desired)?;
        sets_as_maps(self.schema, object, &mut desired);
        if let Some((t, k)) = missing.first() {
            self.fail(format!(
                "{object}: refers to {}, which doesn't exist",
                format_ref(t, k)
            ));
            return Ok(());
        }
        let Some(current) = self.registry.get_singleton(object)? else {
            self.fail(format!("{object}: the server returned nothing"));
            return Ok(());
        };
        match diff(self.schema, object, &desired, &current, &self.options) {
            Err(why) => self.fail(format!("{object} {why}")),
            Ok(patch) if patch.is_empty() => self.outcome.unchanged += 1,
            Ok(patch) => {
                let names: Vec<&str> = patch.keys().map(String::as_str).collect();
                self.say(format!("~ {object}: {}", names.join(", ")));
                if !self.options.dry_run {
                    let answer = self.write(object, json!({ "update": { "singleton": patch } }))?;
                    if let Some(err) = answer.get("notUpdated").and_then(|n| n.get("singleton")) {
                        self.fail(format!("{object}: {}", set_error(err)));
                        return Ok(());
                    }
                }
                self.outcome.updated += 1;
            }
        }
        Ok(())
    }

    fn collection(&mut self, object: &str, items: Vec<Map<String, Value>>) -> Result<()> {
        let existing = self.registry.get_all(object)?;
        self.index.seed(object, &existing);
        let by_id: HashMap<String, &Map<String, Value>> = existing
            .iter()
            .filter_map(|e| Some((e.get("id")?.as_str()?.to_owned(), e)))
            .collect();
        let by_key: HashMap<String, String> = existing
            .iter()
            .filter_map(|e| {
                Some((
                    server_key(self.schema, object, e)?,
                    e.get("id")?.as_str()?.to_owned(),
                ))
            })
            .collect();
        let keys_in_plan: HashSet<String> = items
            .iter()
            .filter_map(|i| plan_key(self.schema, object, i))
            .collect();

        let mut matched: HashSet<String> = HashSet::new();
        let mut pending: Vec<Map<String, Value>> = items;
        loop {
            let mut creates: Vec<(String, Map<String, Value>)> = Vec::new();
            let mut updates: Vec<(String, String, Map<String, Value>)> = Vec::new();
            let mut deferred = Vec::new();
            let mut blocked = Vec::new();

            for item in pending {
                let label = plan_key(self.schema, object, &item)
                    .or_else(|| item.get("@id").and_then(key_text).map(|id| format!("id {id}")))
                    .unwrap_or_else(|| "(no key)".into());
                let mut desired = item.clone();
                let missing = keys_to_refs(&mut self.index, &mut desired)?;
                sets_as_maps(self.schema, object, &mut desired);
                if let Some((t, k)) = missing.first() {
                    // Another object of this type, still to be created, is
                    // fine: it goes in an earlier round.
                    if t == object && keys_in_plan.contains(k) {
                        deferred.push(item);
                    } else {
                        blocked.push(format!(
                            "{object} {label}: refers to {}, which doesn't exist",
                            format_ref(t, k)
                        ));
                    }
                    continue;
                }

                let found = plan_key(self.schema, object, &item)
                    .and_then(|k| by_key.get(&k).cloned())
                    .or_else(|| {
                        item.get("@id")
                            .and_then(Value::as_str)
                            .filter(|id| by_id.contains_key(*id))
                            .map(str::to_owned)
                    });
                match found {
                    Some(id) => {
                        matched.insert(id.clone());
                        match diff(self.schema, object, &desired, by_id[&id], &self.options) {
                            Err(why) => self.fail(format!("{object} {label} {why}")),
                            Ok(patch) if patch.is_empty() => self.outcome.unchanged += 1,
                            Ok(patch) => updates.push((id, label, patch)),
                        }
                    }
                    None => creates.push((label, desired)),
                }
            }
            for line in blocked {
                self.fail(line);
            }

            let progress = !creates.is_empty() || !updates.is_empty();
            self.send_updates(object, updates)?;
            self.send_creates(object, creates)?;

            if deferred.is_empty() {
                break;
            }
            if !progress {
                for item in deferred {
                    let label = plan_key(self.schema, object, &item).unwrap_or_default();
                    self.fail(format!(
                        "{object} {label}: refers to objects of its own type that can't be created first"
                    ));
                }
                break;
            }
            pending = deferred;
        }
        self.matched.push((object.to_owned(), matched, existing));
        Ok(())
    }

    fn send_updates(
        &mut self,
        object: &str,
        updates: Vec<(String, String, Map<String, Value>)>,
    ) -> Result<()> {
        for chunk in updates.chunks(self.registry.max_objects_in_set()) {
            for (_, label, patch) in chunk {
                let names: Vec<&str> = patch.keys().map(String::as_str).collect();
                self.say(format!("~ {object} {label}: {}", names.join(", ")));
            }
            if self.options.dry_run {
                self.outcome.updated += chunk.len();
                continue;
            }
            let update: Map<String, Value> = chunk
                .iter()
                .map(|(id, _, p)| (id.clone(), Value::Object(p.clone())))
                .collect();
            let answer = self.write(object, json!({ "update": update }))?;
            for (id, label, _) in chunk {
                match answer.get("notUpdated").and_then(|n| n.get(id)) {
                    Some(err) => self.fail(format!("{object} {label}: {}", set_error(err))),
                    None => self.outcome.updated += 1,
                }
            }
        }
        Ok(())
    }

    fn send_creates(&mut self, object: &str, creates: Vec<(String, Map<String, Value>)>) -> Result<()> {
        for chunk in creates.chunks(self.registry.max_objects_in_set()) {
            for (label, _) in chunk {
                self.say(format!("+ {object} {label}"));
            }
            if self.options.dry_run {
                for (label, item) in chunk {
                    if let Some(key) = plan_key(self.schema, object, item).or_else(|| Some(label.clone())) {
                        self.index.learn(object, &key, None);
                    }
                }
                self.outcome.created += chunk.len();
                continue;
            }
            let create: Map<String, Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, (_, item))| (format!("c{i}"), Value::Object(creatable(item))))
                .collect();
            let answer = self.write(object, json!({ "create": create }))?;
            for (i, (label, item)) in chunk.iter().enumerate() {
                let cid = format!("c{i}");
                if let Some(err) = answer.get("notCreated").and_then(|n| n.get(&cid)) {
                    self.fail(format!("{object} {label}: {}", set_error(err)));
                    continue;
                }
                let id = answer
                    .get("created")
                    .and_then(|c| c.get(&cid))
                    .and_then(|c| c.get("id"))
                    .and_then(Value::as_str);
                match id {
                    Some(id) => {
                        self.outcome.created += 1;
                        // The key may be one the server sets, such as an
                        // account's address; the plan's @key says what it is.
                        if let Some(key) = plan_key(self.schema, object, item).or_else(|| {
                            answer
                                .get("created")
                                .and_then(|c| c.get(&cid))
                                .and_then(Value::as_object)
                                .and_then(|c| server_key(self.schema, object, c))
                        }) {
                            self.index.learn(object, &key, Some(id));
                        }
                    }
                    None => self.fail(format!("{object} {label}: the server didn't say it was created")),
                }
            }
        }
        Ok(())
    }

    /// Destroys what the plan's collections didn't name, last types first.
    fn prune(&mut self) -> Result<()> {
        let matched = std::mem::take(&mut self.matched);
        for (object, keep, existing) in matched.iter().rev() {
            let doomed: Vec<(String, String)> = existing
                .iter()
                .filter_map(|e| {
                    let id = e.get("id")?.as_str()?;
                    (!keep.contains(id)).then(|| {
                        let label = server_key(self.schema, object, e).unwrap_or_else(|| format!("id {id}"));
                        (id.to_owned(), label)
                    })
                })
                .collect();
            for chunk in doomed.chunks(self.registry.max_objects_in_set()) {
                for (_, label) in chunk {
                    self.say(format!("- {object} {label}"));
                }
                if self.options.dry_run {
                    self.outcome.destroyed += chunk.len();
                    continue;
                }
                let ids: Vec<&str> = chunk.iter().map(|(id, _)| id.as_str()).collect();
                let answer = self.write(object, json!({ "destroy": ids }))?;
                for (id, label) in chunk {
                    match answer.get("notDestroyed").and_then(|n| n.get(id)) {
                        Some(err) => self.fail(format!("{object} {label}: {}", set_error(err))),
                        None => self.outcome.destroyed += 1,
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::tests::sample;
    use std::cell::RefCell;

    /// A registry in memory that behaves like the server for what apply uses.
    struct Fake {
        objects: RefCell<HashMap<String, Vec<Map<String, Value>>>>,
        singletons: RefCell<HashMap<String, Map<String, Value>>>,
        next: RefCell<u32>,
        calls: RefCell<Vec<(String, Value)>>,
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                objects: RefCell::default(),
                singletons: RefCell::default(),
                next: RefCell::new(0),
                calls: RefCell::default(),
            }
        }

        fn insert(&self, object: &str, item: Value) -> String {
            let mut n = self.next.borrow_mut();
            *n += 1;
            let id = format!("i{n}");
            let mut map = item.as_object().unwrap().clone();
            map.insert("id".into(), json!(id));
            if object == "x:Account" {
                // The server sets the address from name and domain.
                let domain = map["domainId"].as_str().unwrap().to_owned();
                let domains = self.objects.borrow();
                let dname = domains["x:Domain"]
                    .iter()
                    .find(|d| d["id"] == json!(domain))
                    .map(|d| d["name"].as_str().unwrap().to_owned())
                    .unwrap();
                map.insert(
                    "emailAddress".into(),
                    json!(format!("{}@{dname}", map["name"].as_str().unwrap())),
                );
            }
            self.objects
                .borrow_mut()
                .entry(object.into())
                .or_default()
                .push(map);
            id
        }
    }

    impl Registry for Fake {
        fn get_all(&self, object: &str) -> Result<Vec<Map<String, Value>>> {
            Ok(self.objects.borrow().get(object).cloned().unwrap_or_default())
        }

        fn get_singleton(&self, object: &str) -> Result<Option<Map<String, Value>>> {
            Ok(Some(
                self.singletons.borrow().get(object).cloned().unwrap_or_default(),
            ))
        }

        fn set(&self, object: &str, args: Value) -> Result<Map<String, Value>> {
            self.calls.borrow_mut().push((object.into(), args.clone()));
            let mut answer = Map::new();
            if let Some(create) = args.get("create").and_then(Value::as_object) {
                let mut created = Map::new();
                for (cid, item) in create {
                    let id = self.insert(object, item.clone());
                    created.insert(cid.clone(), json!({ "id": id }));
                }
                answer.insert("created".into(), Value::Object(created));
            }
            if let Some(update) = args.get("update").and_then(Value::as_object) {
                let mut updated = Map::new();
                for (id, patch) in update {
                    let mut singletons = self.singletons.borrow_mut();
                    let mut objects = self.objects.borrow_mut();
                    let target = if id == "singleton" {
                        singletons.entry(object.into()).or_default()
                    } else {
                        objects
                            .get_mut(object)
                            .and_then(|l| l.iter_mut().find(|o| o["id"] == json!(id)))
                            .unwrap()
                    };
                    for (path, value) in patch.as_object().unwrap() {
                        let mut parts = path.split('/');
                        let first = parts.next().unwrap();
                        match parts.next() {
                            None => {
                                target.insert(first.into(), value.clone());
                            }
                            Some(second) => {
                                target.entry(first).or_insert(json!({}))[second] = value.clone();
                            }
                        }
                    }
                    updated.insert(id.clone(), Value::Null);
                }
                answer.insert("updated".into(), Value::Object(updated));
            }
            if let Some(destroy) = args.get("destroy").and_then(Value::as_array) {
                let mut objects = self.objects.borrow_mut();
                if let Some(list) = objects.get_mut(object) {
                    list.retain(|o| !destroy.contains(&o["id"]));
                }
                answer.insert("destroyed".into(), Value::Array(destroy.clone()));
            }
            answer.insert("x:settingsReload".into(), json!({ "applied": true }));
            Ok(answer)
        }

        fn max_objects_in_set(&self) -> usize {
            2
        }
    }

    fn plan_of(schema: &Schema, v: Value) -> Vec<(String, Value)> {
        read_plan(schema, v).unwrap()
    }

    fn run(fake: &Fake, schema: &Schema, plan: Value, options: Options) -> Outcome {
        let plan = plan_of(schema, plan);
        let mut a = Applier::new(fake, schema, options);
        a.run(&plan).unwrap();
        a.outcome
    }

    fn sample_plan() -> Value {
        json!({
            "Account": [
                {"@type": "User", "@key": "alice@example.com", "name": "alice", "domainId": "@x:Domain/example.com"}
            ],
            "Domain": [{"name": "example.com", "isEnabled": true}, {"name": "example.net"}],
            "Http": {"usePermissiveCors": true}
        })
    }

    #[test]
    fn references_parse_only_with_a_registry_type_and_a_key() {
        assert_eq!(
            parse_ref("@x:Domain/example.com"),
            Some(("x:Domain", "example.com"))
        );
        assert_eq!(
            parse_ref("@x:WebHook/https://h/x"),
            Some(("x:WebHook", "https://h/x"))
        );
        assert_eq!(parse_ref("@x:Domain/"), None);
        assert_eq!(parse_ref("x:Domain/a"), None);
        assert_eq!(parse_ref("@alice"), None);
    }

    #[test]
    fn apply_creates_in_reference_order_then_does_nothing_the_second_time() {
        let schema = sample();
        let fake = Fake::new();
        let first = run(&fake, &schema, sample_plan(), Options::default());
        assert_eq!((first.created, first.updated, first.failed), (3, 1, 0));
        let calls = fake.calls.borrow();
        assert_eq!(
            calls[0].0, "x:Domain",
            "domains before the accounts that need them"
        );
        let account = &fake.objects.borrow()["x:Account"][0];
        let domain_id = fake.objects.borrow()["x:Domain"][0]["id"].clone();
        assert_eq!(account["domainId"], domain_id);
        drop(calls);

        let second = run(&fake, &schema, sample_plan(), Options::default());
        assert_eq!(
            (second.created, second.updated, second.unchanged, second.failed),
            (0, 0, 4, 0)
        );
    }

    #[test]
    fn an_object_can_refer_to_one_of_its_type_created_in_the_same_run() {
        let schema = sample();
        let fake = Fake::new();
        let plan = json!({
            "Domain": [{"name": "example.com"}],
            "Account": [
                {"@type": "User", "@key": "carol@example.com", "name": "carol",
                 "domainId": "@x:Domain/example.com", "memberGroupIds": ["@x:Account/staff@example.com"]},
                {"@type": "Group", "@key": "staff@example.com", "name": "staff",
                 "domainId": "@x:Domain/example.com"}
            ]
        });
        let out = run(&fake, &schema, plan.clone(), Options::default());
        assert_eq!((out.created, out.failed), (3, 0));
        let objects = fake.objects.borrow();
        let staff = objects["x:Account"]
            .iter()
            .find(|a| a["name"] == "staff")
            .unwrap()["id"]
            .clone();
        let carol = objects["x:Account"]
            .iter()
            .find(|a| a["name"] == "carol")
            .unwrap();
        assert_eq!(carol["memberGroupIds"], json!({ staff.as_str().unwrap(): true }));
        drop(objects);
        let again = run(&fake, &schema, plan, Options::default());
        assert_eq!((again.created, again.updated, again.unchanged), (0, 0, 3));
    }

    #[test]
    fn a_dry_run_writes_nothing_and_still_resolves_new_references() {
        let schema = sample();
        let fake = Fake::new();
        let out = run(
            &fake,
            &schema,
            sample_plan(),
            Options {
                dry_run: true,
                ..Default::default()
            },
        );
        assert_eq!((out.created, out.failed), (3, 0));
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn only_changed_properties_are_sent_and_immutable_ones_refuse() {
        let schema = sample();
        let fake = Fake::new();
        fake.insert("x:Domain", json!({"name": "example.com", "isEnabled": false}));
        let out = run(
            &fake,
            &schema,
            json!({"Domain": [{"name": "example.com", "isEnabled": true}]}),
            Options::default(),
        );
        assert_eq!(out.updated, 1);
        let calls = fake.calls.borrow();
        assert_eq!(calls[0].1["update"]["i1"], json!({"isEnabled": true}));
    }

    #[test]
    fn a_missing_reference_fails_that_object_only() {
        let schema = sample();
        let fake = Fake::new();
        let out = run(
            &fake,
            &schema,
            json!({
                "Domain": [{"name": "example.com"}],
                "Account": [{"@type": "User", "@key": "bob@nowhere.test", "name": "bob", "domainId": "@x:Domain/nowhere.test"}]
            }),
            Options::default(),
        );
        assert_eq!((out.created, out.failed), (1, 1));
    }

    #[test]
    fn prune_removes_what_the_plan_leaves_out() {
        let schema = sample();
        let fake = Fake::new();
        fake.insert("x:Domain", json!({"name": "keep.com"}));
        fake.insert("x:Domain", json!({"name": "old.com"}));
        let out = run(
            &fake,
            &schema,
            json!({"Domain": [{"name": "keep.com"}]}),
            Options {
                prune: true,
                ..Default::default()
            },
        );
        assert_eq!((out.destroyed, out.unchanged), (1, 1));
        assert_eq!(fake.objects.borrow()["x:Domain"].len(), 1);
    }

    #[test]
    fn snapshot_writes_references_by_key_and_drops_what_the_server_sets() {
        let schema = sample();
        let fake = Fake::new();
        let d = fake.insert(
            "x:Domain",
            json!({"name": "example.com", "createdAt": "2026-01-01T00:00:00Z"}),
        );
        fake.insert(
            "x:Account",
            json!({"@type": "User", "name": "alice", "domainId": d, "secret": "s"}),
        );
        let types = vec!["x:Domain".to_owned(), "x:Account".to_owned()];
        let (plan, warnings) = snapshot(&fake, &schema, &types).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(plan["x:Domain"], json!([{"name": "example.com"}]));
        assert_eq!(
            plan["x:Account"],
            json!([{"@type": "User", "name": "alice", "domainId": "@x:Domain/example.com", "@key": "alice@example.com"}])
        );

        // Applied back to the same server, a snapshot changes nothing.
        let out = run(&fake, &schema, plan, Options::default());
        assert_eq!((out.created, out.updated, out.failed), (0, 0, 0));
    }

    #[test]
    fn keys_that_repeat_across_namespaces_stay_apart() {
        let schema = sample();
        let fake = Fake::new();
        fake.insert("x:LookupKey", json!({"namespace": "trusted", "key": "li.ru"}));
        fake.insert("x:LookupKey", json!({"namespace": "redirectors", "key": "li.ru"}));
        let plan = json!({"LookupKey": [
            {"namespace": "trusted", "key": "li.ru"},
            {"namespace": "redirectors", "key": "li.ru"}
        ]});
        let out = run(&fake, &schema, plan, Options::default());
        assert_eq!((out.created, out.updated, out.unchanged), (0, 0, 2));
        assert_eq!(
            find_id(&fake, &schema, "x:LookupKey", "redirectors/li.ru").unwrap(),
            "i2"
        );
    }

    #[test]
    fn nested_objects_are_patched_property_by_property() {
        let mut patch = Map::new();
        diff_value(
            "limits",
            &json!({"a": 1, "b": 2}),
            &json!({"a": 1, "b": 3, "c": 4}),
            &mut patch,
        );
        assert_eq!(patch, json!({"limits/b": 2}).as_object().unwrap().clone());
        let mut patch = Map::new();
        diff_value(
            "t",
            &json!({"@type": "B", "x": 1}),
            &json!({"@type": "A", "x": 1}),
            &mut patch,
        );
        assert_eq!(
            patch,
            json!({"t": {"@type": "B", "x": 1}}).as_object().unwrap().clone()
        );
    }

    #[test]
    fn sets_compare_without_order_or_shape() {
        assert!(same_set(&json!(["a", "b"]), &json!({"b": true, "a": true})));
        assert!(!same_set(&json!(["a"]), &json!({"a": true, "b": true})));
    }
}

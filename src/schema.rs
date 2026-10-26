/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! The server's description of its own registry, as `/api/schema` serves it:
//! which types exist, whether each is a collection or a singleton, the
//! properties of each (or of each variant, for a type with several), and how
//! the console lists them.

use serde_json::{Map, Value};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Any number of objects, each with its own id.
    Object,
    /// Exactly one, with the id `singleton`.
    Singleton,
}

/// A property that holds the id of another registry object, or a set of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub property: String,
    pub target: String,
    pub many: bool,
}

/// How a property may be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    Mutable,
    /// Set when the object is created, never changed after.
    Immutable,
    /// The server's to set; never written.
    ServerSet,
}

pub struct Schema {
    root: Value,
}

impl Schema {
    pub fn new(root: Value) -> Result<Self> {
        for key in ["objects", "schemas", "fields"] {
            if !root.get(key).is_some_and(Value::is_object) {
                return Err(Error::Server(format!("the server's schema has no {key}")));
            }
        }
        Ok(Schema { root })
    }

    fn section(&self, key: &str) -> &Map<String, Value> {
        static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        self.root
            .get(key)
            .and_then(Value::as_object)
            .unwrap_or_else(|| EMPTY.get_or_init(Map::new))
    }

    /// The registry types, collections and singletons, in name order.
    pub fn registry_types(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .section("objects")
            .iter()
            .filter(|(name, obj)| {
                name.starts_with("x:")
                    && matches!(
                        obj.get("type").and_then(Value::as_str),
                        Some("object" | "singleton")
                    )
            })
            .map(|(name, _)| name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    /// The registry type a name on the command line means: `Domain`,
    /// `x:Domain` and `domain` are all `x:Domain`; a console view such as
    /// `x:Account/User` is the type it shows.
    pub fn resolve(&self, name: &str) -> Result<String> {
        let objects = self.section("objects");
        let wanted = if name.starts_with("x:") {
            name.to_owned()
        } else {
            format!("x:{name}")
        };
        let found = objects
            .get_key_value(&wanted)
            .map(|(k, _)| k.clone())
            .or_else(|| objects.keys().find(|k| k.eq_ignore_ascii_case(&wanted)).cloned())
            .ok_or_else(|| Error::Usage(format!("no registry type {name}: `inbuxa-cli types` lists them")))?;
        match objects[&found].get("type").and_then(Value::as_str) {
            Some("view") => objects[&found]
                .get("objectName")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| Error::Usage(format!("{name} is a view of no type"))),
            Some("object" | "singleton") => Ok(found),
            _ => Err(Error::Usage(format!("{name} isn't a registry type"))),
        }
    }

    pub fn kind(&self, name: &str) -> Kind {
        match self
            .section("objects")
            .get(name)
            .and_then(|o| o.get("type"))
            .and_then(Value::as_str)
        {
            Some("singleton") => Kind::Singleton,
            _ => Kind::Object,
        }
    }

    pub fn description(&self, name: &str) -> &str {
        self.section("objects")
            .get(name)
            .and_then(|o| o.get("description"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    /// For a type with variants (`@type`), each one's name, label and the
    /// schema that holds its properties.
    pub fn variants(&self, name: &str) -> Vec<(String, String, Option<String>)> {
        let Some(schema) = self.section("schemas").get(name) else {
            return Vec::new();
        };
        if schema.get("type").and_then(Value::as_str) != Some("multiple") {
            return Vec::new();
        }
        schema
            .get("variants")
            .and_then(Value::as_array)
            .map(|vs| {
                vs.iter()
                    .map(|v| {
                        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
                        (
                            s("name").unwrap_or_default(),
                            s("label").unwrap_or_default(),
                            s("schemaName"),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The schema name that holds the properties of `object`, which for a
    /// type with variants depends on its `@type`.
    fn schema_name(&self, name: &str, object: Option<&Map<String, Value>>) -> Option<String> {
        let schema = self.section("schemas").get(name)?;
        match schema.get("type").and_then(Value::as_str) {
            Some("single") => schema
                .get("schemaName")
                .and_then(Value::as_str)
                .map(str::to_owned),
            Some("multiple") => {
                let variant = object?.get("@type")?.as_str()?;
                self.variants(name)
                    .into_iter()
                    .find(|(n, _, _)| n == variant)
                    .and_then(|(_, _, s)| s)
            }
            _ => None,
        }
    }

    /// The properties `object` can have, by name, with each one's spec.
    pub fn properties(&self, name: &str, object: Option<&Map<String, Value>>) -> Option<&Map<String, Value>> {
        let schema_name = self.schema_name(name, object)?;
        self.properties_of_schema(&schema_name)
    }

    pub fn properties_of_schema(&self, schema_name: &str) -> Option<&Map<String, Value>> {
        self.section("fields")
            .get(schema_name)?
            .get("properties")?
            .as_object()
    }

    pub fn update_rule(spec: &Value) -> Update {
        match spec.get("update").and_then(Value::as_str) {
            Some("serverSet") => Update::ServerSet,
            Some("immutable") => Update::Immutable,
            _ => Update::Mutable,
        }
    }

    /// Whether a property holds a secret anywhere inside it: a password in an
    /// account's credentials, a key in a DNS provider's settings. The server
    /// masks these, so they can't be read back or compared.
    pub fn holds_secret(&self, spec: &Value) -> bool {
        self.type_holds_secret(spec.get("type").unwrap_or(&Value::Null), 0)
    }

    fn type_holds_secret(&self, t: &Value, depth: usize) -> bool {
        if matches!(
            t.get("format").and_then(Value::as_str),
            Some("secret" | "secretText")
        ) {
            return true;
        }
        if depth > 8 {
            return false;
        }
        if let Some(class) = t.get("class") {
            return self.type_holds_secret(class, depth + 1);
        }
        let Some(name) = t.get("objectName").and_then(Value::as_str) else {
            return false;
        };
        // A nested object's own type, never a registry reference.
        if t.get("type").and_then(Value::as_str) == Some("objectId") {
            return false;
        }
        let mut schemas: Vec<String> = self.schema_name(name, None).into_iter().collect();
        schemas.extend(self.variants(name).into_iter().filter_map(|(_, _, s)| s));
        schemas
            .iter()
            .filter_map(|s| self.properties_of_schema(s))
            .any(|props| {
                props
                    .values()
                    .any(|p| self.type_holds_secret(p.get("type").unwrap_or(&Value::Null), depth + 1))
            })
    }

    /// The properties of `object` that refer to other registry objects.
    pub fn references(&self, name: &str, object: Option<&Map<String, Value>>) -> Vec<Reference> {
        let Some(props) = self.properties(name, object) else {
            return Vec::new();
        };
        let mut refs: Vec<Reference> = props
            .iter()
            .filter_map(|(prop, spec)| {
                let t = spec.get("type")?;
                let (id_type, many) = match t.get("type").and_then(Value::as_str)? {
                    "objectId" => (t, false),
                    "set" | "list" => (t.get("class")?, true),
                    _ => return None,
                };
                if id_type.get("type").and_then(Value::as_str) != Some("objectId") {
                    return None;
                }
                let target = id_type.get("objectName")?.as_str()?;
                // A view (x:Account/Group) refers to the type it shows.
                let target = target.split('/').next()?;
                target.starts_with("x:").then(|| Reference {
                    property: prop.clone(),
                    target: target.to_owned(),
                    many,
                })
            })
            .collect();
        refs.sort_by(|a, b| a.property.cmp(&b.property));
        refs
    }

    /// Every registry type any variant of `name` refers to.
    pub fn referenced_types(&self, name: &str) -> Vec<String> {
        let mut schemas: Vec<String> = Vec::new();
        if let Some(s) = self.schema_name(name, None) {
            schemas.push(s);
        }
        schemas.extend(self.variants(name).into_iter().filter_map(|(_, _, s)| s));
        let mut targets: Vec<String> = Vec::new();
        for schema in schemas {
            let Some(props) = self.properties_of_schema(&schema) else {
                continue;
            };
            for spec in props.values() {
                let Some(t) = spec.get("type") else { continue };
                let id_type = match t.get("type").and_then(Value::as_str) {
                    Some("objectId") => Some(t),
                    Some("set" | "list") => t.get("class"),
                    _ => None,
                };
                if let Some(target) = id_type
                    .filter(|i| i.get("type").and_then(Value::as_str) == Some("objectId"))
                    .and_then(|i| i.get("objectName"))
                    .and_then(Value::as_str)
                    .and_then(|t| t.split('/').next())
                    .filter(|t| t.starts_with("x:"))
                    && !targets.iter().any(|t| t == target)
                {
                    targets.push(target.to_owned());
                }
            }
        }
        targets
    }

    /// The list the console shows for `name`: its own, or the first of its
    /// views'.
    fn list(&self, name: &str) -> Option<&Map<String, Value>> {
        let lists = self.section("lists");
        lists.get(name).and_then(Value::as_object).or_else(|| {
            let prefix = format!("{name}/");
            let mut views: Vec<(&String, &Value)> =
                lists.iter().filter(|(k, _)| k.starts_with(&prefix)).collect();
            views.sort_by(|a, b| a.0.cmp(b.0));
            views.first().and_then(|(_, v)| v.as_object())
        })
    }

    /// The property that tells one object of a type from another, the way a
    /// person would: a domain's name, an account's address. None for a type
    /// whose objects have nothing of the kind, such as tracers.
    pub fn key_property(&self, name: &str) -> Option<String> {
        let list = self.list(name)?;
        if let Some(label) = list.get("labelProperty").and_then(Value::as_str) {
            return Some(label.to_owned());
        }
        list.get("columns")
            .and_then(Value::as_array)?
            .first()?
            .get("name")?
            .as_str()
            .filter(|n| !n.starts_with('@'))
            .map(str::to_owned)
    }

    /// A property that scopes the key, for a type whose keys repeat from one
    /// namespace to the next (lookup keys: `li.ru` is both a trusted domain
    /// and a URL redirector).
    pub fn key_qualifier(&self, name: &str) -> Option<String> {
        let key = self.key_property(name)?;
        let mut schemas: Vec<String> = self.schema_name(name, None).into_iter().collect();
        schemas.extend(self.variants(name).into_iter().filter_map(|(_, _, s)| s));
        schemas
            .iter()
            .filter_map(|s| self.properties_of_schema(s))
            .any(|props| props.contains_key("namespace"))
            .then(|| "namespace".to_owned())
            .filter(|q| *q != key)
    }

    /// The columns the console shows for a type, by property name.
    pub fn columns(&self, name: &str) -> Vec<String> {
        self.list(name)
            .and_then(|l| l.get("columns"))
            .and_then(Value::as_array)
            .map(|cols| {
                cols.iter()
                    .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub fn sample() -> Schema {
        Schema::new(json!({
            "objects": {
                "x:Domain": {"type": "object", "description": "A domain."},
                "x:Account": {"type": "object"},
                "x:Account/User": {"type": "view", "objectName": "x:Account"},
                "x:Http": {"type": "singleton"},
                "x:Tracer": {"type": "object"},
                "x:LookupKey": {"type": "object"},
                "Mailbox": {"type": "object"}
            },
            "schemas": {
                "x:Domain": {"type": "single", "schemaName": "x:Domain"},
                "x:Http": {"type": "single", "schemaName": "x:Http"},
                "x:Account": {"type": "multiple", "variants": [
                    {"name": "User", "label": "User account", "schemaName": "x:UserAccount"},
                    {"name": "Group", "label": "Group account", "schemaName": "x:GroupAccount"}
                ]},
                "x:LookupKey": {"type": "single", "schemaName": "x:LookupKey"},
                "x:Credential": {"type": "multiple", "variants": [
                    {"name": "Password", "label": "Password", "schemaName": "x:PasswordCredential"}
                ]},
                "x:Tracer": {"type": "multiple", "variants": [
                    {"name": "Stdout", "label": "Console", "schemaName": "x:TracerStdout"}
                ]}
            },
            "fields": {
                "x:Domain": {"properties": {
                    "name": {"type": {"type": "string"}, "update": "immutable"},
                    "isEnabled": {"type": {"type": "boolean"}, "update": "mutable"},
                    "createdAt": {"type": {"type": "utcDateTime"}, "update": "serverSet"}
                }},
                "x:Http": {"properties": {"usePermissiveCors": {"type": {"type": "boolean"}}}},
                "x:UserAccount": {"properties": {
                    "name": {"type": {"type": "string"}, "update": "mutable"},
                    "domainId": {"type": {"type": "objectId", "objectName": "x:Domain"}},
                    "memberGroupIds": {"type": {"type": "set", "class": {"type": "objectId", "objectName": "x:Account/Group"}}},
                    "secret": {"type": {"type": "string", "format": "secret"}},
                    "credentials": {"type": {"type": "objectList", "objectName": "x:Credential"}},
                    "emailAddress": {"type": {"type": "string"}, "update": "serverSet"}
                }},
                "x:GroupAccount": {"properties": {"name": {"type": {"type": "string"}}}},
                "x:TracerStdout": {"properties": {"level": {"type": {"type": "string"}}}},
                "x:PasswordCredential": {"properties": {
                    "secret": {"type": {"type": "string", "format": "secret"}},
                    "expiresAt": {"type": {"type": "utcDateTime"}}
                }},
                "x:LookupKey": {"properties": {
                    "namespace": {"type": {"type": "string"}},
                    "key": {"type": {"type": "string"}}
                }}
            },
            "lists": {
                "x:Domain": {"labelProperty": "name", "columns": [{"name": "name"}, {"name": "isEnabled"}]},
                "x:Account/User": {"columns": [{"name": "emailAddress"}, {"name": "description"}]},
                "x:Tracer": {"columns": [{"name": "@type"}, {"name": "level"}]},
                "x:LookupKey": {"labelProperty": "key", "columns": [{"name": "namespace"}, {"name": "key"}]}
            }
        }))
        .unwrap()
    }

    #[test]
    fn names_resolve_with_or_without_the_prefix_and_through_views() {
        let s = sample();
        assert_eq!(s.resolve("Domain").unwrap(), "x:Domain");
        assert_eq!(s.resolve("x:domain").unwrap(), "x:Domain");
        assert_eq!(s.resolve("Account/User").unwrap(), "x:Account");
        assert!(s.resolve("Nope").is_err());
    }

    #[test]
    fn registry_types_leave_out_views_and_mail_types() {
        assert_eq!(
            sample().registry_types(),
            vec!["x:Account", "x:Domain", "x:Http", "x:LookupKey", "x:Tracer"]
        );
    }

    #[test]
    fn keys_come_from_the_label_or_the_first_column() {
        let s = sample();
        assert_eq!(s.key_property("x:Domain").as_deref(), Some("name"));
        assert_eq!(s.key_property("x:Account").as_deref(), Some("emailAddress"));
        assert_eq!(s.key_property("x:Tracer"), None);
        assert_eq!(s.key_property("x:Http"), None);
    }

    #[test]
    fn secrets_are_found_inside_nested_objects() {
        let s = sample();
        let mut user = Map::new();
        user.insert("@type".into(), json!("User"));
        let props = s.properties("x:Account", Some(&user)).unwrap();
        assert!(s.holds_secret(&props["credentials"]));
        assert!(s.holds_secret(&props["secret"]));
        assert!(!s.holds_secret(&props["domainId"]));
        assert!(!s.holds_secret(&props["name"]));
    }

    #[test]
    fn a_namespace_scopes_the_key() {
        let s = sample();
        assert_eq!(s.key_qualifier("x:LookupKey").as_deref(), Some("namespace"));
        assert_eq!(s.key_qualifier("x:Domain"), None);
    }

    #[test]
    fn references_follow_the_variant() {
        let s = sample();
        let mut user = Map::new();
        user.insert("@type".into(), json!("User"));
        let refs = s.references("x:Account", Some(&user));
        assert_eq!(
            refs,
            vec![
                Reference {
                    property: "domainId".into(),
                    target: "x:Domain".into(),
                    many: false
                },
                Reference {
                    property: "memberGroupIds".into(),
                    target: "x:Account".into(),
                    many: true
                },
            ]
        );
        assert_eq!(s.referenced_types("x:Account"), vec!["x:Domain", "x:Account"]);
    }
}

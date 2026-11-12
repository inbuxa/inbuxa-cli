/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Ergonomic verbs over the generic `create`/`update`: the handful of things an
//! operator does most, with less ceremony.
//!
//!     inbuxa-cli domain add example.com
//!     inbuxa-cli account add alice@example.com
//!     inbuxa-cli group add staff@example.com
//!     inbuxa-cli account set-password alice@example.com
//!     inbuxa-cli account add-alias alice@example.com sales@example.com
//!
//! Each verb builds the same registry object the generic commands do, so there
//! is nothing these can reach that `create`/`update` can't. What they add is
//! the common-case shape: an address splits into a local part and a domain, the
//! domain becomes a reference, a password becomes a credential, an alias an
//! entry in the alias list.
//!
//! The structure is read from the live schema the way the rest of the cli reads
//! it, not hard-coded: the account's variants (`User`, `Group`), the property
//! that refers to a domain, the credential list (the one object list that holds
//! a secret), which credential variant carries a settable password, and the
//! alias list (an object list of addresses). Only the local-part property is
//! taken by its conventional name, `name`, and a verb that can't find it on the
//! account says so and points at `create`.

use clap::Subcommand;
use serde_json::{Map, Value};

use crate::client::Client;
use crate::error::{Error, Result};
use crate::schema::{Schema, Update};

/// The core account concept whose leaf property the schema doesn't single out
/// from other strings, so a verb names it. Guarded: a verb checks the account
/// actually has it before using it.
const LOCAL_PART: &str = "name";
/// Where a password comes from, for the verbs that set one.
const PASSWORD_ENV: &str = "INBUXA_NEW_PASSWORD";

#[derive(Subcommand)]
pub enum AccountCmd {
    /// Create a user account from an address: account add alice@example.com
    Add {
        /// The account's address, local-part@domain
        email: String,
        /// Extra name=value pairs, as `create` takes them
        pairs: Vec<String>,
        /// Set a password too (from INBUXA_NEW_PASSWORD, or asked for)
        #[arg(long)]
        password: bool,
    },
    /// Set or replace an account's password (from INBUXA_NEW_PASSWORD, or asked)
    SetPassword {
        /// The account's address or id
        account: String,
    },
    /// Add an email alias to an account: account add-alias alice@ex.com s@ex.com
    AddAlias {
        /// The account's address or id
        account: String,
        /// The alias address to add, local-part@domain
        alias: String,
    },
}

#[derive(Subcommand)]
pub enum DomainCmd {
    /// Add a domain: domain add example.com
    Add {
        /// The domain name
        name: String,
        /// Extra name=value pairs, as `create` takes them
        pairs: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum GroupCmd {
    /// Add a group account from an address: group add staff@example.com
    Add {
        /// The group's address, local-part@domain
        email: String,
        /// Extra name=value pairs, as `create` takes them
        pairs: Vec<String>,
    },
}

/// `local@domain`, the two halves an address verb works with.
fn split_address(kind: &str, address: &str) -> Result<(String, String)> {
    match address.rsplit_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() && !domain.contains('@') => {
            Ok((local.to_owned(), domain.to_owned()))
        }
        _ => Err(Error::Usage(format!(
            "{address:?} isn't a {kind} address: give it as local-part@domain"
        ))),
    }
}

/// The variant of `x:Account` whose name is `want` (`User`, `Group`), read from
/// the schema rather than assumed.
fn account_variant(schema: &Schema, account: &str, want: &str) -> Result<String> {
    schema
        .variants(account)
        .into_iter()
        .map(|(name, _, _)| name)
        .find(|name| name.eq_ignore_ascii_case(want))
        .ok_or_else(|| {
            Error::Server(format!(
                "this server's {account} has no {want} variant; use `create` instead"
            ))
        })
}

/// A sample object carrying just `@type`, enough for the schema to pick a
/// variant's properties.
fn typed(variant: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("@type".into(), Value::String(variant.to_owned()));
    m
}

/// The type of a property's value, as the schema states it (`string`, `set`,
/// `objectList`, ...).
fn prop_type(spec: &Value) -> Option<&str> {
    spec.get("type")?.get("type")?.as_str()
}

/// The registry type an object-list property holds (`x:Credential`), if it is
/// one.
fn object_list_of(spec: &Value) -> Option<&str> {
    (prop_type(spec)? == "objectList").then(|| spec.get("type")?.get("objectName")?.as_str())?
}

impl Schema {
    // Small, verb-specific lookups kept next to the generic ones.

    /// The account property that refers to a single domain (`domainId`).
    fn domain_property(&self, account: &str, variant: &str) -> Result<String> {
        let sample = typed(variant);
        self.references(account, Some(&sample))
            .into_iter()
            .find(|r| r.target == "x:Domain" && !r.many)
            .map(|r| r.property)
            .ok_or_else(|| Error::Server(format!("this server's {account} has no domain to put it in")))
    }

    /// The account's object-list of credentials: the one that holds a secret.
    /// Returns the property name and the credential type it holds.
    fn credential_list(&self, account: &str, variant: &str) -> Result<(String, String)> {
        let sample = typed(variant);
        let props = self
            .properties(account, Some(&sample))
            .ok_or_else(|| Error::Server(format!("{account} has no properties")))?;
        props
            .iter()
            .find(|(_, spec)| object_list_of(spec).is_some() && self.holds_secret(spec))
            .and_then(|(name, spec)| Some((name.clone(), object_list_of(spec)?.to_owned())))
            .ok_or_else(|| {
                Error::Server(format!(
                    "this server's {account} keeps no password; set one in the console"
                ))
            })
    }

    /// The account's object-list of aliases: an address list, so an object-list
    /// that holds no secret and whose entries carry a domain. Returns the
    /// property name and the alias type.
    fn alias_list(&self, account: &str, variant: &str) -> Result<(String, String)> {
        let sample = typed(variant);
        let props = self
            .properties(account, Some(&sample))
            .ok_or_else(|| Error::Server(format!("{account} has no properties")))?;
        props
            .iter()
            .filter(|(_, spec)| object_list_of(spec).is_some() && !self.holds_secret(spec))
            .find_map(|(name, spec)| {
                let entry = object_list_of(spec)?;
                self.references(entry, None)
                    .iter()
                    .any(|r| r.target == "x:Domain")
                    .then(|| (name.clone(), entry.to_owned()))
            })
            .ok_or_else(|| Error::Server(format!("this server's {account} keeps no alias list")))
    }

    /// The credential variant that carries a password a client sets: the one
    /// whose secret property can be written (an app password's or API key's is
    /// the server's to mint). Returns the variant name and its secret property.
    fn password_variant(&self, credential: &str) -> Result<(String, String)> {
        for (name, _, schema_name) in self.variants(credential) {
            let Some(props) = schema_name.and_then(|s| self.properties_of_schema(&s).cloned()) else {
                continue;
            };
            if let Some(field) = props.iter().find_map(|(field, spec)| {
                (self.holds_secret(spec) && Schema::update_rule(spec) == Update::Mutable)
                    .then(|| field.clone())
            }) {
                return Ok((name, field));
            }
        }
        Err(Error::Server(format!(
            "this server's {credential} has no password that can be set here"
        )))
    }

    /// Confirms the account has the local-part property a verb needs, so the
    /// failure names it rather than the server rejecting a half-built object.
    fn require_local_part(&self, account: &str, variant: &str) -> Result<String> {
        let sample = typed(variant);
        let has = self
            .properties(account, Some(&sample))
            .is_some_and(|p| p.contains_key(LOCAL_PART));
        if has {
            Ok(LOCAL_PART.to_owned())
        } else {
            Err(Error::Server(format!(
                "this server's {account} has no `{LOCAL_PART}`; build it with `create` instead"
            )))
        }
    }
}

/// The index to give a new object-list entry: one past the largest the server
/// already uses, so it is added rather than overwriting one.
fn next_index(list: &Map<String, Value>) -> u64 {
    list.keys()
        .filter_map(|k| k.parse::<u64>().ok())
        .max()
        .map_or(0, |n| n + 1)
}

/// Reads one account the schema's way, by address or id.
fn load_account(client: &Client, schema: &Schema, account: &str, who: &str) -> Result<Map<String, Value>> {
    let id = crate::plan::find_id(client, schema, account, who)?;
    client
        .get(account, &[id])?
        .into_iter()
        .next()
        .ok_or_else(|| Error::Server(format!("no {account} {who}")))
}

pub fn domain(client: &Client, schema: &Schema, cmd: DomainCmd) -> Result<()> {
    let DomainCmd::Add { name, pairs } = cmd;
    let object = schema.resolve("x:Domain")?;
    let key = schema
        .key_property(&object)
        .ok_or_else(|| Error::Server("this server's domain has no name property".into()))?;
    let mut item = crate::values::parse_pairs(&pairs)?;
    item.insert(key, Value::String(name));
    crate::create(client, schema, &object, item)
}

pub fn account(client: &Client, schema: &Schema, cmd: AccountCmd) -> Result<()> {
    match cmd {
        AccountCmd::Add {
            email,
            pairs,
            password,
        } => add_account(client, schema, "User", &email, &pairs, password),
        AccountCmd::SetPassword { account } => set_password(client, schema, &account),
        AccountCmd::AddAlias { account, alias } => add_alias(client, schema, &account, &alias),
    }
}

pub fn group(client: &Client, schema: &Schema, cmd: GroupCmd) -> Result<()> {
    let GroupCmd::Add { email, pairs } = cmd;
    add_account(client, schema, "Group", &email, &pairs, false)
}

/// Shared by `account add` and `group add`: an address, a variant, the rest as
/// pairs.
fn add_account(
    client: &Client,
    schema: &Schema,
    want_variant: &str,
    email: &str,
    pairs: &[String],
    with_password: bool,
) -> Result<()> {
    let object = schema.resolve("x:Account")?;
    let variant = account_variant(schema, &object, want_variant)?;
    let local_part = schema.require_local_part(&object, &variant)?;
    let domain_property = schema.domain_property(&object, &variant)?;
    let (local, domain) = split_address(&want_variant.to_lowercase(), email)?;
    let domain_id = crate::plan::find_id(client, schema, "x:Domain", &domain)?;

    let mut item = crate::values::parse_pairs(pairs)?;
    item.insert("@type".into(), Value::String(variant.clone()));
    item.insert(local_part, Value::String(local));
    item.insert(domain_property, Value::String(domain_id));

    if with_password {
        let (list, credential) = schema.credential_list(&object, &variant)?;
        let (pw_variant, secret_field) = schema.password_variant(&credential)?;
        let secret = crate::secret::from_env_or_prompt(PASSWORD_ENV, "new password")?;
        let mut entry = typed(&pw_variant);
        entry.insert(secret_field, Value::String(secret));
        let mut map = Map::new();
        map.insert("0".into(), Value::Object(entry));
        item.insert(list, Value::Object(map));
    }

    crate::create(client, schema, &object, item)
}

fn set_password(client: &Client, schema: &Schema, who: &str) -> Result<()> {
    let object = schema.resolve("x:Account")?;
    let current = load_account(client, schema, &object, who)?;
    let variant = current
        .get("@type")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Server(format!("{object} {who} has no type")))?
        .to_owned();
    let (list, credential) = schema.credential_list(&object, &variant)?;
    let (pw_variant, secret_field) = schema.password_variant(&credential)?;
    let id = current
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Server(format!("{object} {who} has no id")))?
        .to_owned();
    let secret = crate::secret::from_env_or_prompt(PASSWORD_ENV, "new password")?;

    let credentials = current.get(&list).and_then(Value::as_object);
    let patch = password_patch(&list, &secret_field, &pw_variant, credentials, secret);
    crate::update(client, schema, &object, &id, patch)
}

/// The patch that sets the password: into the existing password credential if
/// there is one, so other credentials and their masked secrets are untouched;
/// otherwise a new credential. Either way one field, the secret, is written.
fn password_patch(
    list: &str,
    secret_field: &str,
    pw_variant: &str,
    credentials: Option<&Map<String, Value>>,
    secret: String,
) -> Map<String, Value> {
    let existing_key = credentials.and_then(|creds| {
        creds.iter().find_map(|(key, entry)| {
            (entry.get("@type").and_then(Value::as_str) == Some(pw_variant)).then(|| key.clone())
        })
    });
    let mut patch = Map::new();
    match existing_key {
        Some(key) => {
            patch.insert(format!("{list}/{key}/{secret_field}"), Value::String(secret));
        }
        None => {
            let index = credentials.map_or(0, next_index);
            let mut entry = typed(pw_variant);
            entry.insert(secret_field.to_owned(), Value::String(secret));
            patch.insert(format!("{list}/{index}"), Value::Object(entry));
        }
    }
    patch
}

fn add_alias(client: &Client, schema: &Schema, who: &str, alias: &str) -> Result<()> {
    let object = schema.resolve("x:Account")?;
    let current = load_account(client, schema, &object, who)?;
    let variant = current
        .get("@type")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Server(format!("{object} {who} has no type")))?
        .to_owned();
    let id = current
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Server(format!("{object} {who} has no id")))?
        .to_owned();
    let (list, alias_type) = schema.alias_list(&object, &variant)?;
    let local_part = schema.require_local_part(&alias_type, "")?;
    let domain_property = schema.domain_property(&alias_type, "")?;

    let (local, domain) = split_address("alias", alias)?;
    let domain_id = crate::plan::find_id(client, schema, "x:Domain", &domain)?;

    let mut entry = Map::new();
    entry.insert(local_part, Value::String(local));
    entry.insert(domain_property, Value::String(domain_id));
    if schema
        .properties(&alias_type, None)
        .is_some_and(|p| p.contains_key("enabled"))
    {
        entry.insert("enabled".into(), Value::Bool(true));
    }

    let aliases = aliases_after_add(current.get(&list).and_then(Value::as_object), entry);
    let mut patch = Map::new();
    patch.insert(list, Value::Object(aliases));
    crate::update(client, schema, &object, &id, patch)
}

/// The alias list with one more entry: the whole list, since writing an object
/// list replaces it, with the new entry at the next free slot. Server-set
/// fields on the existing entries are carried back as they came.
fn aliases_after_add(existing: Option<&Map<String, Value>>, entry: Map<String, Value>) -> Map<String, Value> {
    let mut out = existing.cloned().unwrap_or_default();
    let index = next_index(&out);
    out.insert(index.to_string(), Value::Object(entry));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A schema shaped like the server's for accounts: variants, a domain
    /// reference, a credential list with a settable password and a minted app
    /// password, and an alias list.
    fn schema() -> Schema {
        Schema::new(json!({
            "objects": {
                "x:Domain": {"type": "object"},
                "x:Account": {"type": "object"},
                "x:Account/User": {"type": "view", "objectName": "x:Account"}
            },
            "schemas": {
                "x:Domain": {"type": "single", "schemaName": "x:Domain"},
                "x:Account": {"type": "multiple", "variants": [
                    {"name": "User", "label": "User", "schemaName": "x:UserAccount"},
                    {"name": "Group", "label": "Group", "schemaName": "x:GroupAccount"}
                ]},
                "x:EmailAlias": {"type": "single", "schemaName": "x:EmailAlias"},
                "x:Credential": {"type": "multiple", "variants": [
                    {"name": "Password", "label": "Password", "schemaName": "x:PasswordCredential"},
                    {"name": "AppPassword", "label": "App password", "schemaName": "x:SecondaryCredential"}
                ]}
            },
            "fields": {
                "x:Domain": {"properties": {"name": {"type": {"type": "string"}, "update": "immutable"}}},
                "x:UserAccount": {"properties": {
                    "name": {"type": {"type": "string"}},
                    "description": {"type": {"type": "string"}},
                    "domainId": {"type": {"type": "objectId", "objectName": "x:Domain"}},
                    "emailAddress": {"type": {"type": "string"}, "update": "serverSet"},
                    "aliases": {"type": {"type": "objectList", "objectName": "x:EmailAlias"}},
                    "credentials": {"type": {"type": "objectList", "objectName": "x:Credential"}}
                }},
                "x:GroupAccount": {"properties": {
                    "name": {"type": {"type": "string"}},
                    "domainId": {"type": {"type": "objectId", "objectName": "x:Domain"}},
                    "emailAddress": {"type": {"type": "string"}, "update": "serverSet"},
                    "aliases": {"type": {"type": "objectList", "objectName": "x:EmailAlias"}}
                }},
                "x:EmailAlias": {"properties": {
                    "name": {"type": {"type": "string"}},
                    "domainId": {"type": {"type": "objectId", "objectName": "x:Domain"}},
                    "enabled": {"type": {"type": "boolean"}}
                }},
                "x:PasswordCredential": {"properties": {
                    "secret": {"type": {"type": "string", "format": "secret"}, "update": "mutable"},
                    "expiresAt": {"type": {"type": "utcDateTime"}}
                }},
                "x:SecondaryCredential": {"properties": {
                    "secret": {"type": {"type": "string", "format": "secret"}, "update": "serverSet"}
                }}
            },
            "lists": {
                "x:Domain": {"labelProperty": "name", "columns": [{"name": "name"}]},
                "x:Account/User": {"columns": [{"name": "emailAddress"}]}
            }
        }))
        .unwrap()
    }

    #[test]
    fn an_address_splits_into_a_local_part_and_a_domain() {
        assert_eq!(
            split_address("user", "alice@example.com").unwrap(),
            ("alice".to_owned(), "example.com".to_owned())
        );
        // The last @ wins, so a local part may hold one.
        assert_eq!(
            split_address("user", "a+b@c@example.com").unwrap(),
            ("a+b@c".to_owned(), "example.com".to_owned())
        );
        assert!(split_address("user", "alice").is_err());
        assert!(split_address("user", "@example.com").is_err());
        assert!(split_address("user", "alice@").is_err());
    }

    #[test]
    fn the_account_structure_is_read_from_the_schema() {
        let s = schema();
        assert_eq!(account_variant(&s, "x:Account", "user").unwrap(), "User");
        assert_eq!(account_variant(&s, "x:Account", "GROUP").unwrap(), "Group");
        assert!(account_variant(&s, "x:Account", "robot").is_err());
        assert_eq!(s.domain_property("x:Account", "User").unwrap(), "domainId");
        assert_eq!(
            s.credential_list("x:Account", "User").unwrap(),
            ("credentials".to_owned(), "x:Credential".to_owned())
        );
        assert_eq!(
            s.alias_list("x:Account", "User").unwrap(),
            ("aliases".to_owned(), "x:EmailAlias".to_owned())
        );
        // The settable password is the one with a writable secret.
        assert_eq!(
            s.password_variant("x:Credential").unwrap(),
            ("Password".to_owned(), "secret".to_owned())
        );
        assert_eq!(s.require_local_part("x:Account", "User").unwrap(), "name");
    }

    #[test]
    fn a_new_password_goes_into_the_existing_credential() {
        let creds = json!({
            "0": {"@type": "Password", "secret": "****"},
            "1": {"@type": "AppPassword", "secret": "****"}
        });
        let patch = password_patch(
            "credentials",
            "secret",
            "Password",
            creds.as_object(),
            "new-secret".into(),
        );
        // Patches that one field, so the app password is left alone.
        assert_eq!(
            patch,
            json!({"credentials/0/secret": "new-secret"})
                .as_object()
                .unwrap()
                .clone()
        );
    }

    #[test]
    fn a_first_password_adds_a_credential_past_the_last_one() {
        let creds = json!({"3": {"@type": "AppPassword", "secret": "****"}});
        let patch = password_patch(
            "credentials",
            "secret",
            "Password",
            creds.as_object(),
            "first".into(),
        );
        assert_eq!(
            patch,
            json!({"credentials/4": {"@type": "Password", "secret": "first"}})
                .as_object()
                .unwrap()
                .clone()
        );
        // With no credentials at all, the first slot is 0.
        let patch = password_patch("credentials", "secret", "Password", None, "first".into());
        assert_eq!(
            patch,
            json!({"credentials/0": {"@type": "Password", "secret": "first"}})
                .as_object()
                .unwrap()
                .clone()
        );
    }

    #[test]
    fn an_alias_is_appended_and_keeps_what_is_there() {
        let existing = json!({"0": {"name": "sales", "domainId": "d", "enabled": true}});
        let mut entry = Map::new();
        entry.insert("name".into(), json!("support"));
        entry.insert("domainId".into(), json!("d"));
        let out = aliases_after_add(existing.as_object(), entry);
        assert_eq!(out.len(), 2);
        assert_eq!(out["0"]["name"], json!("sales"));
        assert_eq!(out["1"]["name"], json!("support"));
    }

    #[test]
    fn the_next_index_clears_the_highest_key() {
        assert_eq!(next_index(&Map::new()), 0);
        assert_eq!(next_index(json!({"0": 1, "1": 1}).as_object().unwrap()), 2);
        assert_eq!(next_index(json!({"2": 1, "5": 1}).as_object().unwrap()), 6);
    }
}

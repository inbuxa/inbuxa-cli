/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! The server's management API: JMAP with the registry capability, and the
//! schema that describes every registry type.
//!
//! Registry objects (`x:` types) live on the signed-in account's primary
//! account. Outside DAV the server takes a token, never a password (contract
//! C-23), so an API key is the normal way in; a password works in bootstrap
//! and recovery mode, or when the server runs with INBUXA_HTTP_BASIC_AUTH=all.

use std::time::Duration;

use base64::Engine;
use serde_json::{Map, Value, json};
use ureq::config::RedirectAuthHeaders;
use ureq::tls::{RootCerts, TlsConfig};

use crate::error::{Error, Result};

pub const REGISTRY: &str = "urn:inbuxa:jmap:registry";
const CORE: &str = "urn:ietf:params:jmap:core";

/// The schema is about a megabyte; leave room for it to grow.
const MAX_BODY: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub enum Auth {
    /// An API key (`API_…`) or an OAuth access token.
    Bearer(String),
    /// A user name and password.
    Basic(String, String),
}

impl Auth {
    fn header(&self) -> String {
        match self {
            Auth::Bearer(token) => format!("Bearer {token}"),
            Auth::Basic(user, pass) => format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
            ),
        }
    }
}

/// One method call's answer: its name, its arguments and its call id.
pub type Response = (String, Value, String);

pub struct Client {
    agent: ureq::Agent,
    base: String,
    auth: Auth,
    api_url: String,
    account_id: String,
    pub max_objects_in_get: usize,
    pub max_objects_in_set: usize,
    pub max_calls_in_request: usize,
}

impl Client {
    /// Fetches the JMAP session from `base` and checks the server offers the
    /// registry.
    pub fn connect(base: &str, auth: Auth, insecure: bool) -> Result<Self> {
        let base = base.trim_end_matches('/').to_owned();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err(Error::Usage(format!(
                "{base} isn't a URL: give the server's address with http:// or https://"
            )));
        }
        let tls = TlsConfig::builder()
            .unversioned_rustls_crypto_provider(std::sync::Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .root_certs(RootCerts::PlatformVerifier)
            .disable_verification(insecure)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            // /.well-known/jmap redirects to the session on the same server,
            // and the redirect has to keep the credentials.
            .redirect_auth_headers(RedirectAuthHeaders::SameHost)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(300)))
            .user_agent(concat!("inbuxa-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();

        let mut client = Client {
            agent,
            base,
            auth,
            api_url: "/jmap".into(),
            account_id: String::new(),
            max_objects_in_get: 500,
            max_objects_in_set: 500,
            max_calls_in_request: 16,
        };
        let session = client.get_json("/.well-known/jmap")?;
        client.read_session(&session)?;
        Ok(client)
    }

    fn read_session(&mut self, session: &Value) -> Result<()> {
        // The registry is an account capability: the account that holds it is
        // the one an administrator's session names as its primary for it.
        let primary = session.get("primaryAccounts").and_then(Value::as_object);
        self.account_id = primary
            .and_then(|p| p.get(REGISTRY))
            .and_then(Value::as_str)
            .or_else(|| {
                session.get("accounts").and_then(Value::as_object).and_then(|accounts| {
                    accounts
                        .iter()
                        .find(|(_, a)| {
                            a.get("accountCapabilities")
                                .and_then(Value::as_object)
                                .is_some_and(|c| c.contains_key(REGISTRY))
                        })
                        .map(|(id, _)| id.as_str())
                })
            })
            .ok_or_else(|| {
                Error::Server(format!(
                    "the server doesn't offer {REGISTRY}: either it isn't inbuxa, or this account can't administer it"
                ))
            })?
            .to_owned();
        if let Some(core) = session.get("capabilities").and_then(|c| c.get(CORE)) {
            let limit = |key: &str, default: usize| {
                core.get(key)
                    .and_then(Value::as_u64)
                    .map_or(default, |n| n as usize)
                    .max(1)
            };
            self.max_objects_in_get = limit("maxObjectsInGet", 500);
            self.max_objects_in_set = limit("maxObjectsInSet", 500);
            self.max_calls_in_request = limit("maxCallsInRequest", 16);
        }
        // Only the path: an absolute apiUrl names the server's public
        // address, which isn't necessarily the one we were given.
        if let Some(api) = session.get("apiUrl").and_then(Value::as_str) {
            self.api_url = match api.split_once("://") {
                Some((_, rest)) => rest.find('/').map_or("/jmap".into(), |i| rest[i..].into()),
                None => api.into(),
            };
        }
        Ok(())
    }

    /// The registry's schema: every type, its fields, forms and lists.
    pub fn schema(&self) -> Result<Value> {
        self.get_json("/api/schema")
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut response = self
            .agent
            .get(&url)
            .header("Authorization", self.auth.header())
            .header("Accept", "application/json")
            .call()
            .map_err(|e| Error::Transport(format!("{url}: {e}")))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(|e| Error::Transport(format!("{url}: {e}")))?;
        if status != 200 {
            return Err(http_error(status, &body));
        }
        serde_json::from_str(&body)
            .map_err(|e| Error::Transport(format!("{url} didn't answer with JSON: {e}")))
    }

    /// Sends method calls in one request and returns their answers, in order.
    /// Every call gets the primary account unless it names one.
    pub fn request(&self, calls: Vec<(String, Value)>) -> Result<Vec<Response>> {
        let method_calls: Vec<Value> = calls
            .into_iter()
            .enumerate()
            .map(|(i, (name, mut args))| {
                if let Some(obj) = args.as_object_mut() {
                    obj.entry("accountId")
                        .or_insert_with(|| Value::String(self.account_id.clone()));
                }
                json!([name, args, i.to_string()])
            })
            .collect();
        let body = json!({ "using": [CORE, REGISTRY], "methodCalls": method_calls });
        let url = format!("{}{}", self.base, self.api_url);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", self.auth.header())
            .send_json(&body)
            .map_err(|e| Error::Transport(format!("{url}: {e}")))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(|e| Error::Transport(format!("{url}: {e}")))?;
        if status != 200 {
            return Err(http_error(status, &text));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| Error::Transport(format!("{url} didn't answer with JSON: {e}")))?;
        let responses = value
            .get("methodResponses")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Transport(format!("{url} answered without methodResponses")))?;
        responses
            .iter()
            .map(|r| match r.as_array().map(Vec::as_slice) {
                Some([Value::String(name), args, Value::String(id)]) => {
                    Ok((name.clone(), args.clone(), id.clone()))
                }
                _ => Err(Error::Transport(
                    "a method response isn't [name, args, id]".into(),
                )),
            })
            .collect()
    }

    /// One method call, its answer, and an error if the server answered
    /// `error` instead.
    pub fn call(&self, name: &str, args: Value) -> Result<Value> {
        let mut responses = self.request(vec![(name.to_owned(), args)])?;
        let (answer, args, _) = responses
            .pop()
            .ok_or_else(|| Error::Transport(format!("no answer to {name}")))?;
        if answer == "error" {
            return Err(Error::Server(format!("{name}: {}", method_error(&args))));
        }
        Ok(args)
    }

    /// Every id of a type, a page at a time. Singletons have the one id
    /// `singleton`.
    pub fn all_ids(&self, object: &str, filter: Option<&Value>) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        loop {
            let mut args = json!({ "position": ids.len(), "limit": self.max_objects_in_get });
            if let Some(filter) = filter {
                args["filter"] = filter.clone();
            }
            let answer = self.call(&format!("{object}/query"), args)?;
            let page: Vec<String> = answer
                .get("ids")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            let done = page.len() < self.max_objects_in_get;
            ids.extend(page);
            if done {
                return Ok(ids);
            }
        }
    }

    /// The objects with these ids, in chunks the server accepts. Ids it
    /// doesn't know are left out.
    pub fn get(&self, object: &str, ids: &[String]) -> Result<Vec<Map<String, Value>>> {
        self.get_properties(object, ids, None)
    }

    /// As [`Client::get`], with only the named properties (and the id).
    pub fn get_properties(
        &self,
        object: &str,
        ids: &[String],
        properties: Option<&[&str]>,
    ) -> Result<Vec<Map<String, Value>>> {
        let mut list = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(self.max_objects_in_get) {
            let mut args = json!({ "ids": chunk });
            if let Some(properties) = properties {
                args["properties"] = json!(properties);
            }
            let answer = self.call(&format!("{object}/get"), args)?;
            if let Some(found) = answer.get("list").and_then(Value::as_array) {
                list.extend(found.iter().filter_map(|v| v.as_object().cloned()));
            }
        }
        Ok(list)
    }

    /// Every object of a type.
    pub fn get_all(&self, object: &str) -> Result<Vec<Map<String, Value>>> {
        let ids = self.all_ids(object, None)?;
        self.get(object, &ids)
    }
}

/// The server's account of a failed request: a JMAP problem document when it
/// sent one, otherwise the status.
fn http_error(status: u16, body: &str) -> Error {
    let detail = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        v.get("detail")
            .or_else(|| v.get("title"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let hint = match status {
        401 => {
            " (an API key goes in INBUXA_API_KEY; a password only works in bootstrap or recovery mode, or with INBUXA_HTTP_BASIC_AUTH=all on the server)"
        }
        403 => " (the account lacks the permission for this)",
        _ => "",
    };
    Error::Server(match detail {
        Some(detail) => format!("HTTP {status}: {detail}{hint}"),
        None => format!("HTTP {status}{hint}"),
    })
}

/// A method-level error (`["error", {type, description}, id]`) in words.
pub fn method_error(args: &Value) -> String {
    let kind = args.get("type").and_then(Value::as_str).unwrap_or("error");
    match args.get("description").and_then(Value::as_str) {
        Some(d) if !d.is_empty() => format!("{kind}: {d}"),
        _ => kind.to_owned(),
    }
}

/// A SetError (`notCreated` / `notUpdated` / `notDestroyed` entry) in words.
pub fn set_error(err: &Value) -> String {
    let mut text = method_error(err);
    if let Some(props) = err.get("properties").and_then(Value::as_array) {
        let names: Vec<&str> = props.iter().filter_map(Value::as_str).collect();
        if !names.is_empty() {
            text.push_str(&format!(" [{}]", names.join(", ")));
        }
    }
    if let Some(linked) = err.get("linkedObjects").and_then(Value::as_array) {
        let names: Vec<String> = linked
            .iter()
            .filter_map(|l| {
                Some(format!(
                    "x:{} {}",
                    l.get("object")?.as_str()?,
                    l.get("id")?.as_str()?
                ))
            })
            .collect();
        if !names.is_empty() {
            text.push_str(&format!(" (still used by {})", names.join(", ")));
        }
    }
    if let Some(details) = err.get("validationErrors").or_else(|| err.get("errors"))
        && !details.is_null()
    {
        text.push_str(&format!(" {details}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(api: &str) -> Value {
        json!({
            "capabilities": { CORE: { "maxObjectsInGet": 7, "maxObjectsInSet": 9 } },
            "primaryAccounts": { CORE: "q", REGISTRY: "p" },
            "accounts": { "p": { "accountCapabilities": { REGISTRY: {} } }, "q": {} },
            "apiUrl": api,
        })
    }

    fn bare() -> Client {
        Client {
            agent: ureq::Agent::new_with_defaults(),
            base: "http://h".into(),
            auth: Auth::Bearer("t".into()),
            api_url: String::new(),
            account_id: String::new(),
            max_objects_in_get: 0,
            max_objects_in_set: 0,
            max_calls_in_request: 0,
        }
    }

    #[test]
    fn reads_limits_account_and_api_path() {
        let mut c = bare();
        c.read_session(&session("https://mail.example.com/jmap/"))
            .unwrap();
        assert_eq!(c.account_id, "p");
        assert_eq!(c.api_url, "/jmap/");
        assert_eq!(c.max_objects_in_get, 7);
        assert_eq!(c.max_objects_in_set, 9);
    }

    #[test]
    fn a_relative_api_url_is_kept() {
        let mut c = bare();
        c.read_session(&session("/jmap")).unwrap();
        assert_eq!(c.api_url, "/jmap");
    }

    #[test]
    fn the_registry_account_is_found_without_a_primary() {
        let mut c = bare();
        let mut s = session("/jmap");
        s["primaryAccounts"].as_object_mut().unwrap().remove(REGISTRY);
        c.read_session(&s).unwrap();
        assert_eq!(c.account_id, "p");
    }

    #[test]
    fn a_server_without_the_registry_is_refused() {
        let mut c = bare();
        let mut s = session("/jmap");
        s["primaryAccounts"].as_object_mut().unwrap().remove(REGISTRY);
        s["accounts"]["p"]["accountCapabilities"]
            .as_object_mut()
            .unwrap()
            .remove(REGISTRY);
        assert!(matches!(c.read_session(&s), Err(Error::Server(_))));
    }

    #[test]
    fn basic_auth_is_base64_of_user_and_password() {
        assert_eq!(Auth::Basic("a".into(), "b".into()).header(), "Basic YTpi");
    }

    #[test]
    fn set_errors_name_the_properties() {
        let e = json!({"type": "invalidProperties", "description": "bad", "properties": ["name"]});
        assert_eq!(set_error(&e), "invalidProperties: bad [name]");
    }

    #[test]
    fn set_errors_name_what_still_links_to_an_object() {
        let e = json!({"type": "objectIsLinked", "linkedObjects": [{"object": "DkimSignature", "id": "k1"}]});
        assert_eq!(set_error(&e), "objectIsLinked (still used by x:DkimSignature k1)");
    }
}

/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Getting saved settings applied on the running server.
//!
//! A registry write is stored at once, but most settings take effect only
//! when the server rebuilds its configuration from the registry, which the
//! x:Action reloads do. A current server reloads after every write that needs
//! it and says how that went in the set response's `x:settingsReload`; an
//! older one doesn't, and needs the reload sent. This follows the console's
//! table of which types need which reload.

use serde_json::{Map, Value, json};

use crate::client::{Client, set_error};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    // In the order they run when more than one is due.
    LookupStores,
    TlsCertificates,
    BlockedIps,
    Settings,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::LookupStores => "ReloadLookupStores",
            Action::TlsCertificates => "ReloadTlsCertificates",
            Action::BlockedIps => "ReloadBlockedIps",
            Action::Settings => "ReloadSettings",
        }
    }

    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "settings" => Some(Action::Settings),
            "tls" | "certificates" => Some(Action::TlsCertificates),
            "lookups" => Some(Action::LookupStores),
            "blocked-ips" => Some(Action::BlockedIps),
            _ => None,
        }
    }
}

/// Types the server keeps current without a reload, or that take effect only
/// on a restart (the stores), or that aren't settings at all.
const NOTHING_TO_APPLY: &[&str] = &[
    "Directory",
    "Authentication",
    "Account",
    "AccountPassword",
    "AccountSettings",
    "Alert",
    "ApiKey",
    "AppPassword",
    "DnsServer",
    "Domain",
    "DkimSignature",
    "Enterprise",
    "MailingList",
    "MaskedEmail",
    "OAuthClient",
    "PublicKey",
    "Role",
    "SpamLlm",
    "Tenant",
    "Action",
    "ArchivedItem",
    "ArfExternalReport",
    "Bootstrap",
    "ClusterNode",
    "DmarcExternalReport",
    "DmarcInternalReport",
    "Log",
    "Metric",
    "QueuedMessage",
    "SpamTrainingSample",
    "Task",
    "TlsExternalReport",
    "TlsInternalReport",
    "Trace",
    "BlobStore",
    "Coordinator",
    "DataStore",
    "InMemoryStore",
    "MetricsStore",
    "SearchStore",
    "TracingStore",
    "Application",
];

/// Types a store needs a restart for, which a reload never applies.
pub const RESTART_TYPES: &[&str] = &[
    "x:BlobStore",
    "x:Coordinator",
    "x:DataStore",
    "x:InMemoryStore",
    "x:MetricsStore",
    "x:SearchStore",
    "x:TracingStore",
];

/// The reload that applies a write to `object`, or None when none is needed.
pub fn action_for(object: &str) -> Option<Action> {
    let name = object.strip_prefix("x:")?;
    if NOTHING_TO_APPLY.contains(&name) {
        return None;
    }
    Some(match name {
        "Certificate" => Action::TlsCertificates,
        "StoreLookup" | "HttpLookup" | "MemoryLookupKey" | "MemoryLookupKeyValue" => Action::LookupStores,
        "BlockedIp" => Action::BlockedIps,
        _ => Action::Settings,
    })
}

/// What a set response says about applying its writes, when it says anything.
pub fn server_reload(set_response: &Map<String, Value>) -> Option<(bool, Option<String>)> {
    let reload = set_response.get("x:settingsReload")?;
    let applied = reload.get("applied")?.as_bool()?;
    let description = reload
        .get("description")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .map(str::to_owned);
    Some((applied, description))
}

/// Tracks the writes of one run and applies what the server didn't.
#[derive(Default)]
pub struct Tracker {
    /// Reloads still to send, for writes the server didn't say it applied.
    pending: Vec<Action>,
    /// Reloads the server tried and couldn't apply, with its reason.
    pub failures: Vec<String>,
    /// Store types that were written, which need a restart.
    pub restart: Vec<String>,
}

impl Tracker {
    /// Notes a set response for `object` that changed something.
    pub fn record(&mut self, object: &str, response: &Map<String, Value>) {
        if RESTART_TYPES.contains(&object) && !self.restart.iter().any(|t| t == object) {
            self.restart.push(object.to_owned());
        }
        match server_reload(response) {
            // x:AllowedIp's answer covers the blocked-IP reload only; allowed
            // addresses are rebuilt by a full one.
            Some((true, _)) if object != "x:AllowedIp" => {}
            Some((false, description)) => self.failures.push(format!(
                "{object} was saved but not applied: {}",
                description.unwrap_or_else(|| "the server gave no reason".into())
            )),
            _ => {
                if let Some(action) = action_for(object)
                    && !self.pending.contains(&action)
                {
                    self.pending.push(action);
                }
            }
        }
    }

    /// Sends the reloads still due. Returns what was sent.
    pub fn apply(&mut self, client: &Client) -> Result<Vec<Action>> {
        let mut actions = std::mem::take(&mut self.pending);
        actions.sort();
        if actions.is_empty() {
            return Ok(actions);
        }
        if let Err(e) = send(client, &actions) {
            self.failures.push(e.to_string());
        }
        Ok(actions)
    }
}

/// Runs reload actions on the server, in order.
pub fn send(client: &Client, actions: &[Action]) -> Result<()> {
    let mut create = Map::new();
    for (i, action) in actions.iter().enumerate() {
        create.insert(format!("r{i}"), json!({ "@type": action.name() }));
    }
    let answer = client.call("x:Action/set", json!({ "create": create }))?;
    let not_created = answer.get("notCreated").and_then(Value::as_object);
    let mut problems = Vec::new();
    for (i, action) in actions.iter().enumerate() {
        let key = format!("r{i}");
        let created = answer
            .get("created")
            .and_then(Value::as_object)
            .is_some_and(|c| c.contains_key(&key));
        if created {
            continue;
        }
        let why = not_created
            .and_then(|n| n.get(&key))
            .map(set_error)
            .unwrap_or_else(|| "the server didn't confirm it".into());
        problems.push(format!("{} failed: {why}", action.name()));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::Partial(problems.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_type_gets_the_reload_that_applies_it() {
        assert_eq!(action_for("x:Http"), Some(Action::Settings));
        assert_eq!(action_for("x:Certificate"), Some(Action::TlsCertificates));
        assert_eq!(action_for("x:HttpLookup"), Some(Action::LookupStores));
        assert_eq!(action_for("x:BlockedIp"), Some(Action::BlockedIps));
        assert_eq!(action_for("x:Domain"), None);
        assert_eq!(action_for("x:BlobStore"), None);
        assert_eq!(action_for("Mailbox"), None);
    }

    #[test]
    fn a_server_that_applied_needs_nothing_sent() {
        let mut t = Tracker::default();
        let applied = json!({"x:settingsReload": {"applied": true}});
        t.record("x:Http", applied.as_object().unwrap());
        assert!(t.pending.is_empty() && t.failures.is_empty());
    }

    #[test]
    fn a_server_that_says_nothing_gets_the_reload_sent() {
        let mut t = Tracker::default();
        t.record("x:Http", &Map::new());
        t.record("x:MtaRoute", &Map::new());
        t.record("x:Certificate", &Map::new());
        assert_eq!(t.pending, vec![Action::Settings, Action::TlsCertificates]);
    }

    #[test]
    fn a_failed_reload_is_reported_with_its_reason() {
        let mut t = Tracker::default();
        let failed = json!({"x:settingsReload": {"applied": false, "description": "bad route"}});
        t.record("x:MtaRoute", failed.as_object().unwrap());
        assert_eq!(
            t.failures,
            vec!["x:MtaRoute was saved but not applied: bad route"]
        );
    }

    #[test]
    fn stores_need_a_restart() {
        let mut t = Tracker::default();
        t.record("x:BlobStore", &Map::new());
        assert_eq!(t.restart, vec!["x:BlobStore"]);
        assert!(t.pending.is_empty());
    }
}

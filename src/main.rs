/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! inbuxa-cli: administer an inbuxa server from the command line, and keep
//! its settings in a file.

mod client;
mod error;
mod plan;
mod reload;
mod schema;
mod secret;
mod values;
mod version;

use std::io::{IsTerminal, Write};

/// println! that hands a closed pipe (`| head`) back as an error instead of
/// panicking.
macro_rules! out {
    ($($arg:tt)*) => { writeln!(std::io::stdout(), $($arg)*)? };
}

use clap::{Parser, Subcommand};
use serde_json::{Map, Value, json};

use client::{Auth, Client, set_error};
use error::{Error, Result};
use plan::Registry;
use reload::Tracker;
use schema::{Kind, Schema, Update};

#[derive(Parser)]
#[command(
    name = "inbuxa-cli",
    version = version::shown(),
    about = "Administer an inbuxa server from the command line"
)]
#[command(
    after_help = "Sign in with an API key in INBUXA_API_KEY (asked for when unset), \
or with --user and INBUXA_PASSWORD where the server takes passwords."
)]
struct Cli {
    /// The server's address, such as https://mail.example.com
    #[arg(long, short = 'u', env = "INBUXA_URL", global = true)]
    url: Option<String>,

    /// Sign in with this account's password (from INBUXA_PASSWORD, or asked
    /// for) instead of an API key
    #[arg(long, env = "INBUXA_USER", global = true)]
    user: Option<String>,

    /// Don't check the server's TLS certificate
    #[arg(long, global = true)]
    insecure: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the registry types
    Types,

    /// Show a type's properties, and which of them refer to other types
    Describe { r#type: String },

    /// List the objects of a type
    List {
        r#type: String,
        /// A JMAP filter, as JSON
        #[arg(long)]
        filter: Option<String>,
        /// Print the objects as JSON instead of a table
        #[arg(long)]
        json: bool,
    },

    /// Print objects as JSON, by id or key; all of them if none is named
    Get { r#type: String, ids: Vec<String> },

    /// Create an object from name=value pairs or a JSON file
    Create {
        r#type: String,
        /// name=value; a value that reads as JSON is JSON, anything else a
        /// string. @x:Domain/example.com stands for that domain's id.
        pairs: Vec<String>,
        /// Read the object from this JSON file, or - for standard input
        #[arg(long, short)]
        file: Option<String>,
    },

    /// Change an object's properties (a singleton needs no id)
    Update {
        r#type: String,
        /// The object's id or key, then name=value pairs
        args: Vec<String>,
        /// Read the changes from this JSON file, or - for standard input
        #[arg(long, short)]
        file: Option<String>,
    },

    /// Delete objects by id or key
    Delete {
        r#type: String,
        #[arg(required = true)]
        ids: Vec<String>,
    },

    /// Apply saved settings on the running server
    Reload {
        /// settings, tls, lookups or blocked-ips; settings if none
        what: Vec<String>,
    },

    /// Write the registry's settings as a plan
    Snapshot {
        /// The types to include; every settings type if none
        types: Vec<String>,
        /// Write the plan to this file instead of standard output
        #[arg(long, short)]
        output: Option<String>,
    },

    /// Make the server match a plan
    Apply {
        /// The plan, or - for standard input
        file: String,
        /// Show what would change, and change nothing
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Also delete objects of the plan's types that the plan leaves out
        #[arg(long)]
        prune: bool,
        /// Don't ask before pruning
        #[arg(long, short)]
        yes: bool,
        /// Write secrets to objects that exist (they're only set on create
        /// otherwise, since the server never shows them to compare)
        #[arg(long)]
        write_secrets: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        if let Error::Io(io) = &e
            && io.kind() == std::io::ErrorKind::BrokenPipe
        {
            return;
        }
        eprintln!("inbuxa-cli: {e}");
        std::process::exit(e.exit_code());
    }
}

fn connect(cli: &Cli) -> Result<(Client, Schema)> {
    let url = cli
        .url
        .as_deref()
        .ok_or_else(|| Error::Usage("which server? Give --url or set INBUXA_URL".into()))?;
    let auth = match &cli.user {
        Some(user) => Auth::Basic(
            user.clone(),
            secret::from_env_or_prompt("INBUXA_PASSWORD", "password")?,
        ),
        None => Auth::Bearer(secret::from_env_or_prompt("INBUXA_API_KEY", "API key")?),
    };
    let client = Client::connect(url, auth, cli.insecure)?;
    let schema = Schema::new(client.schema()?)?;
    Ok((client, schema))
}

fn run(cli: Cli) -> Result<()> {
    let (client, schema) = connect(&cli)?;
    match cli.command {
        Command::Types => types(&schema),
        Command::Describe { r#type } => describe(&schema, &schema.resolve(&r#type)?),
        Command::List { r#type, filter, json } => {
            let filter = filter
                .map(|f| {
                    serde_json::from_str::<Value>(&f).map_err(|e| Error::Usage(format!("--filter: {e}")))
                })
                .transpose()?;
            list(&client, &schema, &schema.resolve(&r#type)?, filter.as_ref(), json)
        }
        Command::Get { r#type, ids } => get(&client, &schema, &schema.resolve(&r#type)?, &ids),
        Command::Create { r#type, pairs, file } => {
            let object = schema.resolve(&r#type)?;
            create(&client, &schema, &object, properties(&pairs, file.as_deref())?)
        }
        Command::Update { r#type, args, file } => {
            let object = schema.resolve(&r#type)?;
            let (id, pairs) = match schema.kind(&object) {
                Kind::Singleton => ("singleton".to_owned(), &args[..]),
                Kind::Object => match args.split_first() {
                    Some((target, rest)) if !target.contains('=') => {
                        (plan::find_id(&client, &schema, &object, target)?, rest)
                    }
                    _ => return Err(Error::Usage(format!("which {object}? Give its id or key first"))),
                },
            };
            update(
                &client,
                &schema,
                &object,
                &id,
                properties(pairs, file.as_deref())?,
            )
        }
        Command::Delete { r#type, ids } => delete(&client, &schema, &schema.resolve(&r#type)?, &ids),
        Command::Reload { what } => {
            let mut actions = Vec::new();
            for word in if what.is_empty() {
                vec!["settings".to_owned()]
            } else {
                what
            } {
                actions.push(reload::Action::from_word(&word).ok_or_else(|| {
                    Error::Usage(format!(
                        "can't reload {word}: settings, tls, lookups or blocked-ips"
                    ))
                })?);
            }
            actions.sort();
            actions.dedup();
            reload::send(&client, &actions)?;
            for a in actions {
                out!("{}: done", a.name());
            }
            Ok(())
        }
        Command::Snapshot { types, output } => snapshot(&client, &schema, &types, output.as_deref()),
        Command::Apply {
            file,
            dry_run,
            prune,
            yes,
            write_secrets,
        } => apply(
            &client,
            &schema,
            &file,
            plan::Options {
                dry_run,
                prune,
                write_secrets,
            },
            yes,
        ),
    }
}

/// The object a command line describes: a file, pairs, or pairs over a file.
fn properties(pairs: &[String], file: Option<&str>) -> Result<Map<String, Value>> {
    let mut object = match file {
        Some(path) => values::read_object(path)?,
        None => Map::new(),
    };
    object.extend(values::parse_pairs(pairs)?);
    if object.is_empty() {
        return Err(Error::Usage("nothing given: name=value pairs, or --file".into()));
    }
    Ok(object)
}

fn print_json(value: &Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

fn types(schema: &Schema) -> Result<()> {
    let names = schema.registry_types();
    let width = names.iter().map(|n| n.len()).max().unwrap_or(0);
    for name in names {
        let kind = match schema.kind(name) {
            Kind::Singleton => "singleton",
            Kind::Object => "",
        };
        let description = schema.description(name).lines().next().unwrap_or("");
        out!("{name:width$}  {kind:9}  {description}");
    }
    Ok(())
}

fn describe(schema: &Schema, object: &str) -> Result<()> {
    out!(
        "{object}{}",
        if schema.kind(object) == Kind::Singleton {
            " (singleton)"
        } else {
            ""
        }
    );
    let description = schema.description(object);
    if !description.is_empty() {
        out!("{description}");
    }
    if let Some(key) = schema.key_property(object) {
        out!("key: {key}");
    }
    let variants = schema.variants(object);
    let mut sections: Vec<(String, Option<Map<String, Value>>)> = Vec::new();
    if variants.is_empty() {
        sections.push((String::new(), None));
    } else {
        for (name, label, _) in variants {
            let mut sample = Map::new();
            sample.insert("@type".into(), Value::String(name.clone()));
            sections.push((format!("@type {name} ({label})"), Some(sample)));
        }
    }
    for (heading, sample) in sections {
        out!();
        if !heading.is_empty() {
            out!("{heading}");
        }
        let Some(props) = schema.properties(object, sample.as_ref()) else {
            out!("  (no properties)");
            continue;
        };
        let refs = schema.references(object, sample.as_ref());
        let width = props.keys().map(String::len).max().unwrap_or(0);
        for (name, spec) in props {
            let t = spec.get("type");
            let mut kind = t
                .and_then(|t| t.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned();
            if let Some(class) = t
                .and_then(|t| t.get("class"))
                .and_then(|c| c.get("type"))
                .and_then(Value::as_str)
            {
                kind = format!("{kind} of {class}");
            }
            if let Some(r) = refs.iter().find(|r| &r.property == name) {
                kind = format!("{kind} -> {}", r.target);
            }
            let mut notes = Vec::new();
            match Schema::update_rule(spec) {
                Update::ServerSet => notes.push("server-set"),
                Update::Immutable => notes.push("set on create"),
                Update::Mutable => {}
            }
            if schema.holds_secret(spec) {
                notes.push("secret");
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!("  ({})", notes.join(", "))
            };
            out!("  {name:width$}  {kind}{notes}");
        }
    }
    Ok(())
}

/// A cell of the list table.
fn cell(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => format!("[{}]", a.len()),
        Some(Value::Object(o)) => match o.get("@type") {
            Some(Value::String(t)) => t.clone(),
            _ => format!("{{{}}}", o.len()),
        },
        Some(other) => other.to_string(),
    }
}

fn list(client: &Client, schema: &Schema, object: &str, filter: Option<&Value>, as_json: bool) -> Result<()> {
    let items = if schema.kind(object) == Kind::Singleton {
        client.get(object, &["singleton".to_owned()])?
    } else {
        let ids = client.all_ids(object, filter)?;
        client.get(object, &ids)?
    };
    if as_json {
        return print_json(&Value::Array(items.into_iter().map(Value::Object).collect()));
    }
    let mut columns = vec!["id".to_owned()];
    for c in schema.columns(object) {
        if !columns.contains(&c) {
            columns.push(c);
        }
    }
    if columns.len() == 1 {
        columns.push("@type".into());
    }
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|i| columns.iter().map(|c| cell(i.get(c))).collect())
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(n, c)| {
            rows.iter()
                .map(|r| r[n].chars().count())
                .chain([c.len()])
                .max()
                .unwrap_or(0)
                .min(48)
        })
        .collect();
    let line = |cells: &[String]| -> Result<()> {
        let parts: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| {
                let c: String = c.chars().take(*w).collect();
                format!("{c:w$}")
            })
            .collect();
        out!("{}", parts.join("  ").trim_end());
        Ok(())
    };
    line(&columns)?;
    for row in &rows {
        line(row)?;
    }
    eprintln!("{} {object}", rows.len());
    Ok(())
}

fn get(client: &Client, schema: &Schema, object: &str, names: &[String]) -> Result<()> {
    let items = match schema.kind(object) {
        Kind::Singleton => client.get(object, &["singleton".to_owned()])?,
        Kind::Object if names.is_empty() => client.get_all(object)?,
        Kind::Object => {
            let ids = names
                .iter()
                .map(|n| plan::find_id(client, schema, object, n))
                .collect::<Result<Vec<_>>>()?;
            client.get(object, &ids)?
        }
    };
    match (schema.kind(object), names.len(), items.len()) {
        (Kind::Singleton, _, _) | (Kind::Object, 1, 1) => {
            print_json(&Value::Object(items.into_iter().next().unwrap_or_default()))
        }
        _ => print_json(&Value::Array(items.into_iter().map(Value::Object).collect())),
    }
}

/// Applies what a write needs applying, and says what's left to do.
fn finish(client: &Client, mut tracker: Tracker) -> Result<()> {
    for action in tracker.apply(client)? {
        eprintln!("sent {}", action.name());
    }
    for t in &tracker.restart {
        eprintln!("note: {t} takes effect when the server restarts");
    }
    if tracker.failures.is_empty() {
        Ok(())
    } else {
        Err(Error::Partial(tracker.failures.join("\n")))
    }
}

fn create(client: &Client, schema: &Schema, object: &str, mut item: Map<String, Value>) -> Result<()> {
    if schema.kind(object) == Kind::Singleton {
        return Err(Error::Usage(format!("{object} is a singleton: use update")));
    }
    plan::resolve_refs(client, schema, object, &mut item)?;
    let answer = Registry::set(client, object, json!({ "create": { "new": item } }))?;
    if let Some(err) = answer.get("notCreated").and_then(|n| n.get("new")) {
        return Err(Error::Server(format!("{object} not created: {}", set_error(err))));
    }
    let id = answer
        .get("created")
        .and_then(|c| c.get("new"))
        .and_then(|c| c.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Server("the server didn't say what it created".into()))?;
    out!("{id}");
    let mut tracker = Tracker::default();
    tracker.record(object, &answer);
    finish(client, tracker)
}

fn update(
    client: &Client,
    schema: &Schema,
    object: &str,
    id: &str,
    mut patch: Map<String, Value>,
) -> Result<()> {
    plan::resolve_refs(client, schema, object, &mut patch)?;
    let answer = Registry::set(client, object, json!({ "update": { id: patch } }))?;
    if let Some(err) = answer.get("notUpdated").and_then(|n| n.get(id)) {
        return Err(Error::Server(format!(
            "{object} {id} not updated: {}",
            set_error(err)
        )));
    }
    eprintln!("updated {object} {id}");
    let mut tracker = Tracker::default();
    tracker.record(object, &answer);
    finish(client, tracker)
}

fn delete(client: &Client, schema: &Schema, object: &str, names: &[String]) -> Result<()> {
    if schema.kind(object) == Kind::Singleton {
        return Err(Error::Usage(format!(
            "{object} is a singleton and can't be deleted"
        )));
    }
    let ids = names
        .iter()
        .map(|n| plan::find_id(client, schema, object, n))
        .collect::<Result<Vec<_>>>()?;
    let answer = Registry::set(client, object, json!({ "destroy": ids }))?;
    let mut problems = Vec::new();
    for (id, name) in ids.iter().zip(names) {
        match answer.get("notDestroyed").and_then(|n| n.get(id)) {
            Some(err) => problems.push(format!("{object} {name} not deleted: {}", set_error(err))),
            None => eprintln!("deleted {object} {name}"),
        }
    }
    let mut tracker = Tracker::default();
    if problems.len() < ids.len() {
        tracker.record(object, &answer);
    }
    finish(client, tracker)?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::Partial(problems.join("\n")))
    }
}

fn snapshot(client: &Client, schema: &Schema, names: &[String], output: Option<&str>) -> Result<()> {
    let types: Vec<String> = if names.is_empty() {
        schema
            .registry_types()
            .into_iter()
            .filter(|t| !plan::NOT_SETTINGS.contains(t))
            .map(str::to_owned)
            .collect()
    } else {
        names.iter().map(|n| schema.resolve(n)).collect::<Result<_>>()?
    };
    let (plan, warnings) = plan::snapshot(client, schema, &types)?;
    for w in warnings {
        eprintln!("note: {w}");
    }
    let mut text = serde_json::to_string_pretty(&plan)?;
    text.push('\n');
    match output {
        Some(path) => std::fs::write(path, text).map_err(|e| Error::Usage(format!("{path}: {e}")))?,
        None => std::io::stdout().lock().write_all(text.as_bytes())?,
    }
    Ok(())
}

fn apply(client: &Client, schema: &Schema, file: &str, options: plan::Options, yes: bool) -> Result<()> {
    let text = if file == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(file).map_err(|e| Error::Usage(format!("{file}: {e}")))?
    };
    let value: Value = serde_json::from_str(&text).map_err(|e| Error::Usage(format!("{file}: {e}")))?;
    let plan = plan::read_plan(schema, value)?;

    if options.prune && !options.dry_run && !yes {
        if !std::io::stdin().is_terminal() || file == "-" {
            return Err(Error::Usage(
                "--prune deletes objects: add --yes, or try --dry-run first".into(),
            ));
        }
        let types: Vec<&str> = plan.iter().map(|(t, _)| t.as_str()).collect();
        eprint!(
            "--prune deletes every {} the plan leaves out. Go ahead? [y/N] ",
            types.join(", ")
        );
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err(Error::Usage("nothing changed".into()));
        }
    }

    let dry_run = options.dry_run;
    let mut applier = plan::Applier::new(client, schema, options);
    applier.run(&plan)?;
    let o = &applier.outcome;
    let verb = if dry_run { "would be " } else { "" };
    eprintln!(
        "{} {verb}created, {} {verb}updated, {} {verb}deleted, {} unchanged{}",
        o.created,
        o.updated,
        o.destroyed,
        o.unchanged,
        if o.failed > 0 {
            format!(", {} failed", o.failed)
        } else {
            String::new()
        }
    );
    let failed = o.failed;
    let tracker = std::mem::take(&mut applier.tracker);
    if !dry_run {
        finish(client, tracker)?;
    }
    if failed > 0 {
        return Err(Error::Partial(format!("{failed} change(s) failed")));
    }
    Ok(())
}

/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! inbuxa-cli: administer an inbuxa server from the command line, and keep
//! its settings in a file.

mod client;
mod error;
mod events;
mod output;
mod plan;
mod queue;
mod reload;
mod report;
mod schema;
mod secret;
mod store;
mod values;
mod verbs;
mod version;

use std::io::{IsTerminal, Write};

/// println! that hands a closed pipe (`| head`) back as an error instead of
/// panicking.
macro_rules! out {
    ($($arg:tt)*) => { writeln!(std::io::stdout(), $($arg)*)? };
}

use clap::{CommandFactory, Parser, Subcommand};
use serde_json::{Map, Value, json};

use client::{Auth, Client, set_error};
use error::{Error, Result};
use output::Format;
use plan::Registry;
use reload::Tracker;
use report::{detail, note};
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

    /// How to print results: table, json or yaml (table unless given, except
    /// `get`, which is json)
    #[arg(long, short = 'o', global = true, value_name = "FORMAT")]
    output: Option<Format>,

    /// Say less: print results and errors, nothing else
    #[arg(long, short = 'q', global = true, conflicts_with = "verbose")]
    quiet: bool,

    /// Say more about what's happening
    #[arg(long, short = 'v', global = true)]
    verbose: bool,

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

    /// Add and manage accounts: add, set-password, add-alias
    #[command(subcommand)]
    Account(verbs::AccountCmd),

    /// Add a domain
    #[command(subcommand)]
    Domain(verbs::DomainCmd),

    /// Add a group account
    #[command(subcommand)]
    Group(verbs::GroupCmd),

    /// Print the DNS records a domain needs, as a zone file
    Dns {
        /// The domain's name or id
        domain: String,
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
        #[arg(long, value_name = "FILE")]
        to: Option<String>,
    },

    /// Check the store, take snapshots, and copy after a blob store change
    #[command(subcommand)]
    Store(store::StoreCommand),

    /// See and act on the outbound SMTP queue
    #[command(subcommand)]
    Queue(queue::QueueCommand),

    /// Tail the server's log live: trace events as lines
    Logs {
        /// Keep only events that match these words
        #[arg(long)]
        filter: Option<String>,
        /// Narrow by a field, as key=value; may be given more than once
        pairs: Vec<String>,
    },

    /// Tail the trace stream as JSON, one event a line, for jq and grep
    Events {
        /// Keep only events that match these words
        #[arg(long)]
        filter: Option<String>,
        /// Narrow by a field, as key=value; may be given more than once
        pairs: Vec<String>,
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

    /// Print a shell completion script to standard output
    ///
    /// Source it, or install it where the shell looks:
    ///   bash: inbuxa-cli completions bash > /etc/bash_completion.d/inbuxa-cli
    ///   zsh:  inbuxa-cli completions zsh  > "${fpath[1]}/_inbuxa-cli"
    ///   fish: inbuxa-cli completions fish > ~/.config/fish/completions/inbuxa-cli.fish
    Completions {
        /// The shell to generate for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Print a man page, in roff, to standard output
    ///
    ///   inbuxa-cli man > /usr/share/man/man1/inbuxa-cli.1
    Man,
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
    detail!(
        "inbuxa-cli: {url}, as {}",
        match &cli.user {
            Some(user) => user.as_str(),
            None => "an API key",
        }
    );
    let client = Client::connect(url, auth, cli.insecure)?;
    let schema = Schema::new(client.schema()?)?;
    detail!("inbuxa-cli: {} registry types", schema.registry_types().len());
    Ok((client, schema))
}

fn run(cli: Cli) -> Result<()> {
    report::set(if cli.quiet {
        report::Level::Quiet
    } else if cli.verbose {
        report::Level::Verbose
    } else {
        report::Level::Normal
    });

    // Completions and the man page are generated from the command itself and
    // reach no server, so they come before connecting.
    if let Command::Completions { shell } = cli.command {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        return Ok(());
    }
    if let Command::Man = cli.command {
        clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
        return Ok(());
    }
    let (client, schema) = connect(&cli)?;
    match cli.command {
        Command::Types => types(&schema, Format::or(cli.output, Format::Table)),
        Command::Describe { r#type } => describe(
            &schema,
            &schema.resolve(&r#type)?,
            Format::or(cli.output, Format::Table),
        ),
        Command::List { r#type, filter } => {
            let filter = filter
                .map(|f| {
                    serde_json::from_str::<Value>(&f).map_err(|e| Error::Usage(format!("--filter: {e}")))
                })
                .transpose()?;
            list(
                &client,
                &schema,
                &schema.resolve(&r#type)?,
                filter.as_ref(),
                Format::or(cli.output, Format::Table),
            )
        }
        Command::Get { r#type, ids } => get(
            &client,
            &schema,
            &schema.resolve(&r#type)?,
            &ids,
            Format::or(cli.output, Format::Json),
        ),
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
        Command::Account(cmd) => verbs::account(&client, &schema, cmd),
        Command::Domain(cmd) => verbs::domain(&client, &schema, cmd),
        Command::Group(cmd) => verbs::group(&client, &schema, cmd),
        Command::Dns { domain } => dns(&client, &schema, &domain),
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
        Command::Snapshot { types, to } => snapshot(&client, &schema, &types, to.as_deref()),
        Command::Store(command) => store::run(&client, command),
        Command::Queue(command) => queue::run(&client, &schema, command),
        Command::Logs { filter, pairs } => events::run(&client, events::View::Logs, filter, &pairs),
        Command::Events { filter, pairs } => events::run(&client, events::View::Events, filter, &pairs),
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
        // Handled before connecting.
        Command::Completions { .. } | Command::Man => unreachable!(),
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

/// Writes a value to standard output in a structured format.
fn emit(format: Format, value: &Value) -> Result<()> {
    let text = match format {
        Format::Json => output::json(value),
        Format::Yaml => output::yaml(value),
        Format::Table => unreachable!("a table is rendered by the command, not here"),
    };
    std::io::stdout().write_all(text.as_bytes())?;
    Ok(())
}

/// The registry types as structured data, for `types -o json|yaml`.
fn types_value(schema: &Schema) -> Value {
    Value::Array(
        schema
            .registry_types()
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "kind": match schema.kind(name) {
                        Kind::Singleton => "singleton",
                        Kind::Object => "object",
                    },
                    "description": schema.description(name).lines().next().unwrap_or(""),
                })
            })
            .collect(),
    )
}

fn types(schema: &Schema, format: Format) -> Result<()> {
    if format != Format::Table {
        return emit(format, &types_value(schema));
    }
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

/// One property as structured data, mirroring the `describe` table's columns.
fn property_values(schema: &Schema, object: &str, sample: Option<&Map<String, Value>>) -> Vec<Value> {
    let Some(props) = schema.properties(object, sample) else {
        return Vec::new();
    };
    let refs = schema.references(object, sample);
    props
        .iter()
        .map(|(name, spec)| {
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
            let mut p = Map::new();
            p.insert("name".into(), json!(name));
            p.insert("type".into(), json!(kind));
            if let Some(r) = refs.iter().find(|r| &r.property == name) {
                p.insert("references".into(), json!(r.target));
            }
            match Schema::update_rule(spec) {
                Update::ServerSet => {
                    p.insert("update".into(), json!("serverSet"));
                }
                Update::Immutable => {
                    p.insert("update".into(), json!("immutable"));
                }
                Update::Mutable => {}
            }
            if schema.holds_secret(spec) {
                p.insert("secret".into(), json!(true));
            }
            Value::Object(p)
        })
        .collect()
}

/// A type's shape as structured data, for `describe -o json|yaml`.
fn describe_value(schema: &Schema, object: &str) -> Value {
    let mut root = Map::new();
    root.insert("type".into(), json!(object));
    root.insert("singleton".into(), json!(schema.kind(object) == Kind::Singleton));
    if let Some(key) = schema.key_property(object) {
        root.insert("key".into(), json!(key));
    }
    let variants = schema.variants(object);
    if variants.is_empty() {
        root.insert(
            "properties".into(),
            Value::Array(property_values(schema, object, None)),
        );
    } else {
        let vs: Vec<Value> = variants
            .iter()
            .map(|(name, label, _)| {
                let mut sample = Map::new();
                sample.insert("@type".into(), json!(name));
                json!({
                    "@type": name,
                    "label": label,
                    "properties": property_values(schema, object, Some(&sample)),
                })
            })
            .collect();
        root.insert("variants".into(), Value::Array(vs));
    }
    Value::Object(root)
}

fn describe(schema: &Schema, object: &str, format: Format) -> Result<()> {
    if format != Format::Table {
        return emit(format, &describe_value(schema, object));
    }
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

/// Renders objects as the console's columns: id and the type's list columns.
fn table(schema: &Schema, object: &str, items: &[Map<String, Value>]) -> Result<()> {
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
    Ok(())
}

fn list(
    client: &Client,
    schema: &Schema,
    object: &str,
    filter: Option<&Value>,
    format: Format,
) -> Result<()> {
    let items = if schema.kind(object) == Kind::Singleton {
        client.get(object, &["singleton".to_owned()])?
    } else {
        let ids = client.all_ids(object, filter)?;
        client.get(object, &ids)?
    };
    let count = items.len();
    match format {
        Format::Table => table(schema, object, &items)?,
        _ => emit(
            format,
            &Value::Array(items.into_iter().map(Value::Object).collect()),
        )?,
    }
    note!("{count} {object}");
    Ok(())
}

fn get(client: &Client, schema: &Schema, object: &str, names: &[String], format: Format) -> Result<()> {
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
    if format == Format::Table {
        return table(schema, object, &items);
    }
    let value = match (schema.kind(object), names.len(), items.len()) {
        (Kind::Singleton, _, _) | (Kind::Object, 1, 1) => {
            Value::Object(items.into_iter().next().unwrap_or_default())
        }
        _ => Value::Array(items.into_iter().map(Value::Object).collect()),
    };
    emit(format, &value)
}

/// Prints the DNS records a domain needs: the zone file the server keeps
/// ready for it, with its current DKIM key, as a domain's DNS page shows them.
fn dns(client: &Client, schema: &Schema, domain: &str) -> Result<()> {
    let object = schema.resolve("x:Domain")?;
    let id = plan::find_id(client, schema, &object, domain)?;
    let items = client.get_properties(&object, &[id], Some(&["dnsZoneFile"]))?;
    let zone = items
        .first()
        .and_then(|d| d.get("dnsZoneFile"))
        .and_then(Value::as_str)
        .filter(|z| !z.trim().is_empty())
        .ok_or_else(|| Error::Server(format!("the server has no DNS records for {domain} yet")))?;
    let mut out = std::io::stdout().lock();
    out.write_all(zone.as_bytes())?;
    if !zone.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(())
}

/// Applies what a write needs applying, and says what's left to do.
fn finish(client: &Client, mut tracker: Tracker) -> Result<()> {
    for action in tracker.apply(client)? {
        note!("sent {}", action.name());
    }
    for t in &tracker.restart {
        note!("note: {t} takes effect when the server restarts");
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
    note!("updated {object} {id}");
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
            None => note!("deleted {object} {name}"),
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
        note!("note: {w}");
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
    note!(
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::Shell;

    #[test]
    fn the_command_tree_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn completions_generate_for_every_shell() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let mut cmd = Cli::command();
            let mut out = Vec::new();
            clap_complete::generate(shell, &mut cmd, "inbuxa-cli", &mut out);
            let script = String::from_utf8(out).unwrap();
            assert!(!script.is_empty(), "{shell} produced nothing");
            assert!(script.contains("snapshot"), "{shell} is missing a subcommand");
        }
    }

    #[test]
    fn the_man_page_renders() {
        let mut out = Vec::new();
        clap_mangen::Man::new(Cli::command()).render(&mut out).unwrap();
        let page = String::from_utf8(out).unwrap();
        assert!(
            page.contains("inbuxa-cli"),
            "the man page should name the program"
        );
        assert!(
            page.contains("SUBCOMMANDS"),
            "the man page should list the subcommands"
        );
    }
}

# inbuxa-cli

> [!NOTE]
> Development happens on [git.coffeylabs.org/inbuxa/inbuxa-cli](https://git.coffeylabs.org/inbuxa/inbuxa-cli); the copy on GitHub is a read-only mirror.
> Report issues at **[git.coffeylabs.org/inbuxa/inbuxa-cli/issues](https://git.coffeylabs.org/inbuxa/inbuxa-cli/issues)**, and join discussions at **[community.coffeylabs.org](https://community.coffeylabs.org)**.

Administer an **inbuxa** server from the command line, and keep its settings
in a file.

    inbuxa-cli list Domain                         what's there
    inbuxa-cli create Domain name=example.com      add to it
    inbuxa-cli snapshot -o server.json             the settings, as a plan
    inbuxa-cli apply server.json --dry-run         what a plan would change

Everything the console manages lives in the server's registry: domains,
accounts, listeners, spam rules, tracers, about a hundred types in all.
inbuxa-cli reads the server's own description of those types, so it knows
every one the server knows, including types added after this program was
built.

## Installing

Download the archive for your machine from the
[releases](https://git.coffeylabs.org/inbuxa/inbuxa-cli/releases), check it
against `SHA256SUMS`, and put `inbuxa-cli` on your `PATH`:

    curl -LO https://git.coffeylabs.org/inbuxa/inbuxa-cli/releases/latest/download/inbuxa-cli-linux-amd64.tar.gz
    curl -LO https://git.coffeylabs.org/inbuxa/inbuxa-cli/releases/latest/download/SHA256SUMS
    sha256sum --check --ignore-missing SHA256SUMS
    tar xzf inbuxa-cli-linux-amd64.tar.gz
    sudo install inbuxa-cli /usr/local/bin/

Or build it: `cargo build --release` leaves it in `target/release/`.

`SHA256SUMS` is signed with [minisign](https://jedisct1.github.io/minisign/),
so you can confirm it came from us before trusting the checksums.
`SHA256SUMS.minisig` is the signature:

    curl -LO https://git.coffeylabs.org/inbuxa/inbuxa-cli/releases/latest/download/SHA256SUMS.minisig
    minisign -Vm SHA256SUMS -P RWS1yFYwVuJ7MGJTZXLbhv9+hrEsjrC8Zg+7WmPlM86Sjy0qF/xkoCKq

The same public key signs every inbuxa release (installer, migrate and cli).

## Signing in

Give the server's address with `--url` or `INBUXA_URL`, and an API key in
`INBUXA_API_KEY`. Without one, inbuxa-cli asks for it. Make the key in the
console under **Account › Credentials › API Keys**, signed in as an
administrator.

    export INBUXA_URL=https://mail.example.com
    export INBUXA_API_KEY=API_…
    inbuxa-cli types

The server takes passwords on its management API only in bootstrap and
recovery mode, or when it runs with `INBUXA_HTTP_BASIC_AUTH=all`. There,
`--user admin@example.com` signs in with the password in `INBUXA_PASSWORD`,
or asks for it. Secrets never go on the command line, where they would end up
in the shell's history.

`--insecure` skips the TLS certificate check, for a test server with a
self-signed certificate.

## Commands

A type is named as the console's settings pages name it, with or without the
`x:` prefix: `Domain`, `x:Domain` and `domain` are the same. An object is
named by its id or by its key: a domain by its name, an account by its
address.

| Command | Does |
| --- | --- |
| `types` | lists the registry types |
| `describe TYPE` | shows a type's properties: which are set on create only, which the server sets, which are secret, which refer to other types |
| `list TYPE [--filter JSON] [--json]` | lists the objects of a type |
| `get TYPE [ID…]` | prints objects as JSON |
| `create TYPE name=value… [--file F]` | creates an object and prints its id |
| `update TYPE [ID] name=value… [--file F]` | changes properties; a singleton needs no id |
| `delete TYPE ID…` | deletes objects |
| `domain add NAME [name=value…]` | adds a domain |
| `account add EMAIL [name=value…] [--password]` | adds a user account from an address |
| `group add EMAIL [name=value…]` | adds a group account from an address |
| `account set-password EMAIL` | sets or replaces an account's password |
| `account add-alias EMAIL ALIAS` | adds an email alias to an account |
| `dns DOMAIN` | prints the DNS records a domain needs, as a zone file |
| `reload [settings\|tls\|lookups\|blocked-ips]` | applies saved settings on the running server |
| `snapshot [TYPE…] [-o FILE]` | writes the settings as a plan |
| `apply FILE [--dry-run] [--prune] [--yes] [--write-secrets]` | makes the server match a plan |
| `store …` | checks the store, takes snapshots, copies after a blob store change: see [The store](#the-store) |
| `completions SHELL` | prints a shell completion script: see [Shell completions](#shell-completions) |
| `man` | prints a man page, in roff: see [Shell completions](#shell-completions) |

A value that reads as JSON is JSON (`true`, `42`, `["a","b"]`,
`{"@type":"Stdout"}`); anything else is a string, so `name=example.com` needs
no quotes. A name with slashes, such as `limits/maxSize=10`, changes one
property inside an object. To refer to another object, write its type and
key: `domainId=@x:Domain/example.com`.

    inbuxa-cli create Account @type=User name=alice domainId=@x:Domain/example.com
    inbuxa-cli update Account alice@example.com description="Alice Liddell"
    inbuxa-cli update Http usePermissiveCors=false
    inbuxa-cli delete Account alice@example.com

`dns` prints the records a domain needs, as a zone file with its current DKIM
key, the same ones the installer writes to `dns.zone`. Pipe it where your DNS
lives, or read it to set the records by hand:

    inbuxa-cli dns example.com

Most settings take effect when the server reloads its configuration. A
current server does that after every write that needs it, and inbuxa-cli
reports when a reload failed. An older one doesn't, and inbuxa-cli sends the
reload itself. Stores (data, blob, search and the rest) take effect only when
the server restarts, and inbuxa-cli says so.

## Shortcuts for the common case

A few verbs do what `create` and `update` do, with less to type. An account
is given by its address, which splits into a name and a domain, and a password
is read from `INBUXA_NEW_PASSWORD` or asked for, never the command line.

    inbuxa-cli domain add example.com
    inbuxa-cli account add alice@example.com --password
    inbuxa-cli group add staff@example.com
    inbuxa-cli account set-password alice@example.com
    inbuxa-cli account add-alias alice@example.com sales@example.com

`account add` and `group add` take extra `name=value` pairs, as `create`
does, so `account add alice@example.com description="Alice Liddell"` sets the
full name in the same step. The verbs read the account's shape from the
server, so they fit whatever this server's accounts, credentials and aliases
are; for anything they don't cover, `create` and `update` reach every
property.

## The store

The server's store check, store snapshots and blob store changes (server
27.1-rc.16 or 2026.11.9.1 and later; docs.inbuxa.org, *Checking the store,
snapshots, and moving blobs*). They need the matching permissions:
`sysStoreCheck`, `sysStoreSnapshot` and `sysBlobStoreChange`.

| Command | Does |
| --- | --- |
| `store check [--full] [--blobs-per-second N] [--bytes-per-second N] [--no-wait] [--json]` | starts a check, waits, prints what it found; exits 1 when it found problems |
| `store checks [ID] [--json]` | lists the latest checks, or shows one with its problems |
| `store stop-check ID` | stops a running check |
| `store snapshot [--no-wait] [--json]` | takes a snapshot, waits, prints its directory |
| `store snapshots [--json]` | lists the snapshots on disk |
| `store delete-snapshot ID` | deletes a snapshot's directory |
| `store changes [ID] [--json]` | lists the blob store changes, or shows one with what a copy couldn't copy |
| `store plan FILE` | plans a change to the blob store setting in FILE (`-` for standard input), and prints its id |
| `store copy ID [--blobs-per-second N] [--bytes-per-second N] [--no-wait]` | copies what the old blob store still has, or a planned change's blobs ahead to its store, waits; exits 1 when some blobs couldn't be copied |
| `store apply ID` | writes a planned change's store to the blob store setting, once it's copied ahead |
| `store stop-copy ID` | stops a running copy |
| `store clean-up ID [--dry-run] [--yes] [--blobs-per-second N] [--bytes-per-second N] [--no-wait]` | deletes from a settled change's old store each blob the current store has and checks out, waits; `--dry-run` only counts; asks first unless `--yes`; exits 1 when some blobs stayed |
| `store stop-clean-up ID` | stops a running cleanup or preview |
| `store dismiss ID [--yes]` | forgets a change; asks first when it isn't settled |

A nightly check from cron, mailed when it finds something or can't run:

    inbuxa-cli store check --full > /tmp/store-check.txt || mail -s "store check" root < /tmp/store-check.txt

A backup job on the server's machine (the path is on the server) that copies
the new snapshot, and copies nothing when there isn't one:

    dir=$(inbuxa-cli store snapshot) && rsync -a "$dir/" backup:/srv/inbuxa/

A refusal (another data store than RocksDB, a blob store change that isn't
settled, one already being taken) exits 4 with the server's reason.

Moving the blob store with as little reading through as possible (server
27.1-rc.18 and later): plan the change with the new store's settings, as
`x:BlobStore` takes them, copy it ahead while the old store is still in use,
and apply it. When each node restarts on the new setting, the plan becomes
the change, and a last copy picks up what was written in between.

    id=$(inbuxa-cli store plan s3.json)
    inbuxa-cli store copy "$id"
    inbuxa-cli store apply "$id"
    # restart every node, then:
    inbuxa-cli store copy "$id"

The file holds credentials, so keep it out of version control and delete
it after.

Once a change is settled, the old store still has a copy of every blob it
held (server 27.1-rc.19 and later keep it so it can be cleaned up). Backups
of the data store taken before the change read their blobs from it, so
clean it up once those backups have aged out. Preview first:

    inbuxa-cli store clean-up "$id" --dry-run
    inbuxa-cli store clean-up "$id"

A blob whose copy in the current store doesn't check out stays in the old
store and is listed; put it right (a full `store check` finds it) and run
the cleanup again.

## Plans

A plan is the registry written down as JSON: a type and its objects, or a
singleton and its properties.

```json
{
  "x:Domain": [
    { "name": "example.com", "isEnabled": true }
  ],
  "x:Account": [
    { "@type": "Group", "@key": "staff@example.com", "name": "staff",
      "domainId": "@x:Domain/example.com" },
    { "@type": "User", "@key": "alice@example.com", "name": "alice",
      "domainId": "@x:Domain/example.com",
      "memberGroupIds": ["@x:Account/staff@example.com"] }
  ],
  "x:Http": { "usePermissiveCors": false }
}
```

`snapshot` writes one from a server, and `apply` makes a server match one. Keep
the server's settings in version control, review a change as a diff, and
apply the same plan to a second server to set it up the same way.

- **Matching.** Objects are matched by key: the property that names them, such
  as a domain's `name`. Where the server sets the key itself, as with an
  account's address, the plan carries it as `@key`. Lookup keys repeat from
  one list to the next, so their key is `namespace/key`. A type whose objects
  have no name, such as tracers, is matched by `@id`, which ties those
  entries to the server they came from.
- **References.** Ids differ from one server to the next, so a property that
  refers to another object holds `@<type>/<key>`. `apply` creates what's
  referred to before what refers to it, including objects of the same type
  (a group before its members).
- **Only what's named.** `apply` changes only the properties a plan names and
  leaves the rest as they are. Inside a nested object it compares property by
  property. A list or set is replaced whole.
- **Secrets.** The server never shows a secret, so a snapshot has none: no
  passwords, no API keys, no DNS provider credentials. Add them to a plan by
  hand. `apply` writes them when it creates an object, but not to one that
  exists, since it can't compare them, unless you pass `--write-secrets`.
- **Set once.** A property that can only be set on create (a domain's name) is
  reported, not changed. Delete the object and apply again.
- **Deleting.** `--prune` deletes the objects of each type in the plan that
  the plan leaves out. It asks first, or takes `--yes`. Try `--dry-run` before
  either.

A snapshot leaves out what isn't settings: reports, logs, queued messages,
tasks and other records, people's own credentials, and DKIM keys, which the
server makes for each domain. Name a type to include it anyway:
`inbuxa-cli snapshot DkimSignature`.

`apply` prints a line per change (`+` created, `~` updated, `-` deleted, `!`
failed) and a count of each at the end.

## Shell completions

`inbuxa-cli completions SHELL` prints a completion script to standard output,
for `bash`, `zsh`, `fish`, `powershell` or `elvish`. It reaches no server.
Install it where the shell looks:

    inbuxa-cli completions bash > /etc/bash_completion.d/inbuxa-cli
    inbuxa-cli completions zsh  > "${fpath[1]}/_inbuxa-cli"
    inbuxa-cli completions fish > ~/.config/fish/completions/inbuxa-cli.fish

The script completes the commands above and their flags. Type and object
names come from the server, so they aren't completed offline.

`inbuxa-cli man` prints a man page, in roff, to standard output:

    inbuxa-cli man > /usr/share/man/man1/inbuxa-cli.1

## Exit status

| Status | Means |
| --- | --- |
| 0 | done |
| 1 | a store check or a copy finished and found problems |
| 2 | the command line or the plan asks for something that can't work |
| 3 | the server couldn't be reached |
| 4 | the server refused the request: bad credentials, a missing permission |
| 5 | some changes failed; the rest were made |

## Testing

    cargo test

The unit tests need no server. `apply` is tested against a registry held in
memory. Before a release, the commands are also run by hand against a
throwaway server in a container: a snapshot applied back to the server it
came from changes nothing, and one applied after its objects are deleted
recreates them with every reference intact.

## License

AGPL-3.0-only. The text is in [LICENSES](LICENSES).

Copyright (C) 2026, Coffey Labs LLC

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
| `reload [settings\|tls\|lookups\|blocked-ips]` | applies saved settings on the running server |
| `snapshot [TYPE…] [-o FILE]` | writes the settings as a plan |
| `apply FILE [--dry-run] [--prune] [--yes] [--write-secrets]` | makes the server match a plan |

A value that reads as JSON is JSON (`true`, `42`, `["a","b"]`,
`{"@type":"Stdout"}`); anything else is a string, so `name=example.com` needs
no quotes. A name with slashes, such as `limits/maxSize=10`, changes one
property inside an object. To refer to another object, write its type and
key: `domainId=@x:Domain/example.com`.

    inbuxa-cli create Account @type=User name=alice domainId=@x:Domain/example.com
    inbuxa-cli update Account alice@example.com description="Alice Liddell"
    inbuxa-cli update Http usePermissiveCors=false
    inbuxa-cli delete Account alice@example.com

Most settings take effect when the server reloads its configuration. A
current server does that after every write that needs it, and inbuxa-cli
reports when a reload failed. An older one doesn't, and inbuxa-cli sends the
reload itself. Stores (data, blob, search and the rest) take effect only when
the server restarts, and inbuxa-cli says so.

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

## Exit status

| Status | Means |
| --- | --- |
| 0 | done |
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

# Changelog

Versions follow the inbuxa suite: a series (`27.1`), its patches (`27.1.3`)
and release candidates before a series ships (`27.1-rc.1`). The tag is the
version with a `v`. Releases before 27.1 were dated.

## [27.1-rc.3]

Seeing and steering the server: the outbound queue, a live trace, DNS records,
signed downloads, ergonomic verbs, output formats and a shared profile.

- `queue`: list, show, hold, release, retry and drop messages in the server's
  outbound SMTP queue. (#9)
- `events`: tail the server's live trace stream, filtered by type. (#10)
- Releases are signed: the published `SHA256SUMS` carries a
  `SHA256SUMS.minisig` from the suite's minisign key (public key in the
  README), so a download can be checked for authenticity. (#11)
- `completions <shell>` prints a shell completion script (bash, zsh, fish,
  powershell or elvish), and `man` prints a man page in roff. Both are
  generated from the command itself and reach no server. (#12)
- Ergonomic verbs over create and update: `domain add`, `account add`,
  `account set-password`, `account add-alias` and `group add`, so the common
  changes need no hand-written object. (#13)
- `dns <domain>`: print a domain's DNS records as a zone file -- SPF, MX,
  DMARC, the current DKIM key, MTA-STS and the rest, live from the server.
  (#14)
- `-o json|yaml|table` picks the output format of the data commands (`list`,
  `get`, `describe`, `types`), and `--quiet` or `--verbose` set how much
  status is printed. (#15)
- A shared profile, `~/.config/inbuxa/config.toml`, supplies the server
  address and API key so they need not be given each time. `[profile.<name>]`
  sections hold a `url` and either an `api-key` or an `api-key-file` (a path,
  so the key can live in its own file); `--profile` or `$INBUXA_PROFILE` pick
  a section, the default being `default`. A `--url`/`--user` flag or an
  `INBUXA_*` variable still wins. (#16)

## [27.1-rc.2]

The store: checks, snapshots and blob store changes from the command line.

- `store`: the server's store check, store snapshots and blob store changes
  (server 27.1-rc.16 or 2026.11.9.1 and later). `store check` and
  `store snapshot` wait for the result; a check or a copy that found
  problems exits 1, and a refusal says why. (#4)
- `store plan` and `store apply`: plan a blob store change before the
  setting moves, copy it ahead with `store copy`, and write it to the
  setting (server 27.1-rc.18 and later). The new store's settings come from
  a file or standard input, never the command line.
- `store clean-up`: delete from a settled change's old store what the
  current store has and checks out, after a `--dry-run` preview; it asks
  first, since older backups need the old store (server 27.1-rc.19 and
  later). `store stop-clean-up` stops one.

## [27.1-rc.1]

The first release candidate of the 27.1 series, with the rest of the suite.

- `--version` and the User-Agent show the version as tags name it
  (`27.1-rc.1`).

## [2026.10.26]

The first release.

- Lists, reads, creates, updates and deletes any registry type the server
  describes, naming objects by id or by key.
- `snapshot` writes the server's settings as a plan; `apply` makes a server
  match one, with references between objects written by key so a plan moves
  between servers.
- Settings are applied on the running server after a change, with a note when
  a store needs a restart.

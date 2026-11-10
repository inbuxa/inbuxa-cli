# Changelog

Versions follow the inbuxa suite: a series (`27.1`), its patches (`27.1.3`)
and release candidates before a series ships (`27.1-rc.1`). The tag is the
version with a `v`. Releases before 27.1 were dated.

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

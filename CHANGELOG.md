# Changelog

Versions follow the inbuxa suite: a series (`27.1`), its patches (`27.1.3`)
and release candidates before a series ships (`27.1-rc.1`). The tag is the
version with a `v`. Releases before 27.1 were dated.

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

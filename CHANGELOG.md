# Changelog

## [2026.10.26]

The first release.

- Lists, reads, creates, updates and deletes any registry type the server
  describes, naming objects by id or by key.
- `snapshot` writes the server's settings as a plan; `apply` makes a server
  match one, with references between objects written by key so a plan moves
  between servers.
- Settings are applied on the running server after a change, with a note when
  a store needs a restart.

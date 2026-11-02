# ic3-cli

`ic3` is a Microsoft Teams chat command line for agents. It signs in once with a device code, then
sends messages and streams message events as JSON, with no prompts. It is built on the `ic3-teams`
library from [ic3](https://github.com/Jezza/ic3).

> **Unofficial.** It talks to Teams' internal, undocumented chat service as the signed-in account.
> Microsoft does not support it, and it can break at any time. Use a dedicated account for agents.

Linux only.

## Install

```sh
cargo install --path .
```

## Commands

```sh
ic3 login                      # device-code sign-in; open the URL on any device and enter the code
ic3 login --force              # sign in again even if the session still works
ic3 logout                     # forget the session
ic3 list                       # id<TAB>kind<TAB>name, most recently active first
ic3 list --json                # one JSON object per line
ic3 send '<id>' 'deploy *finished*'
echo 'from stdin' | ic3 send '<id>' -   # stdin must be a pipe or file, not a terminal
ic3 send '<id>' 'literal *stars*' --plain
ic3 send '<id>' 'hi' --json    # prints the receipt
ic3 listen                     # every conversation
ic3 listen '<id1>,<id2>' '<id3>'
```

Global options: `--session <path>` (env `IC3_SESSION`, default
`$XDG_DATA_HOME/ic3-cli/session.json`), `--tenant <t>` (env `IC3_TENANT`, default `organizations`),
`-v`/`-vv` for more log output. `RUST_LOG` overrides the log level. Logs and error lines go to
stderr and are redacted.

Only `login` is interactive. Every other command fails fast if there is no usable session.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Any other failure; one line on stderr says what |
| 2 | Not signed in, or the session expired: run `ic3 login` |

### `list`

`kind` is `chat` (1:1), `group`, `channel` or `notes`. With `--json`:

```json
{"id":"19:…@thread.v2","kind":"group","name":"Launch plans","hidden":false,"last_active":"2026-11-02T09:14:03.123Z"}
```

`last_active` is null when unknown. Channels are named `Team/Channel`.

### `send`

The text is parsed for `*bold*`, `_italic_`, `~strike~`, `` `code` ``, lists and quotes unless
`--plain` is given. Trailing line breaks are dropped and an empty message is an error. `--json`
prints:

```json
{"conversation":"48:notes","client_message_id":"…","arrived_at":"2026-11-02T09:14:03.123Z"}
```

`arrived_at` is left out when Teams did not return one.

### `listen`

One JSON object per line on stdout, flushed after each:

```json
{"type":"message","conversation":"19:…","id":"1762074843123","client_message_id":"…","thread_root":null,"from":"8:orgid:…","from_name":"Jane Smith","self":false,"arrived_at":"2026-11-02T09:14:03.123Z","text":"plain text","reply_to":null,"attachments":[]}
{"type":"edit", …the same fields; id is the original message's…}
{"type":"delete","conversation":"19:…","id":"1762074843123"}
```

- `self` is true for messages the signed-in account sent. `client_message_id` matches the one
  `send --json` printed.
- `thread_root` is the post a channel reply sits under. `reply_to` is `{"id","from_name","text"}`
  for a quoted reply. `attachments` are `{"name","kind"}` with kind `file`, `image` or `other`.
- It exits 0 on SIGINT or SIGTERM, even mid-write to a reader that has stopped reading, and as
  soon as the reader of its stdout pipe goes away.
- Reconnects happen by themselves. Only live events are streamed: if Teams reports that events may
  have been lost, a warning goes to stderr and nothing is replayed.
- An expired session exits with code 2.

## Running `listen` and `send` together

Both can share one session file. The library keeps the refresh token in memory and saves each new
one back, about hourly. Three sibling files and rules keep that safe:

- **`session.json.lock`.** Every read and write of the session holds an exclusive lock on it, so
  concurrent saves never collide.
- **`session.json.gen`.** `ic3 login` and `ic3 logout` write a new sign-in generation here. A process
  started under an older generation never saves or clears the session again. A running `listen`
  checks every 30 seconds: after a logout it exits with code 2, and after a new login it reconnects
  with the new session. So a logout sticks, and a login as another account is never overwritten
  by the old one.
- **Routine rotations.** Within one sign-in, the last process to save wins. Entra does not revoke a
  refresh token when it is used, so every process's token keeps working. If one is ever refused,
  `ic3` does not delete a session another process has since saved. It rebuilds its client from the
  file and carries on.

## Checking it against a live account

1. `ic3 login`, sign in as the agent account, then `ic3 login` again: it says "already signed in".
2. `ic3 list` shows Notes, chats and channels with names.
3. `ic3 send 48:notes 'hello *there*'` exits 0 and the message appears in the account's Notes.
4. `ic3 listen` in one terminal; post, edit and delete a message to the account from the official
   client. Three lines appear: `message`, `edit`, `delete`.
5. With `listen` still running, `ic3 send <id> test --json`. The `listen` line for it has
   `"self":true` and the same `client_message_id`.
6. Leave `listen` running for over an hour, which spans a token renewal, then repeat step 5.
7. With `listen` running, `ic3 logout`. Within 30 seconds `listen` exits with code 2, and the session
   file stays gone after the next hourly renewal would have happened.

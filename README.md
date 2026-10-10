# luv

English | [日本語](README-ja.md)

luv is a version control system. Like git or jj, it records changes as commits, keeps a history, and integrates work with three-way merges. The difference is the unit it manages: not files made of lines, but objects that have IDs.

It is alpha. Commands, APIs and the store format will change without compatibility.

## What kind of VCS it is

### It manages objects

An object is one of these:

- text: a text document.
- record: a structured value with fields and collections, close to JSON.
- binary: a byte string. The bytes live in a separate blob.
- conflict: a merge result that has not been resolved.
- deleted: the state that says an object was removed.

An object has a random ID that is independent of any path. A path is just one piece of metadata, the `title`, and is not used for identity. The core is built on this. The CLI, however, currently maps a file to an object by using its path as the `title`, so it does not track a file that is moved.

### Merges look at the shape of the data

A three-way merge (base / left / right) is a pure function: the same input always gives the same result.

- Text is merged by line with diff3 on top of a histogram diff. Line endings and whitespace are not normalized; bytes are taken as they are.
- A record is merged per field path. The schema says, for each path, whether it is handled as Scalar, Set, List, Text or Opaque.
- Elements of a collection have stable IDs. A Set merges adds and removals per element. A List also merges element order, and raises an order conflict only when both sides reordered it differently.

A conflict is not written into the file as markers. It is recorded as a `Conflict` object that keeps each disagreement with its kind: value, order, deleted-vs-edited, text region, or metadata (both sides renamed the object differently, say). You resolve it by taking one side, or by giving the text or the record directly, with a title when the title is in conflict.

### Operations are part of the history

Every operation runs in one SQLite transaction, so a crash leaves either all of it or nothing. There is an operation log, and undo / redo work on it.

A commit can have several parents. There are no branch names or bookmarks, and several heads can exist side by side.

### IDs are keyed hashes

Content IDs (content / state / tree / commit / conflict / blob) are HMAC-SHA256 under a per-repo key, over canonical CBOR, so the same value always encodes to the same bytes. A snapshot is a Patricia tree keyed by object ID. It has the same root regardless of insertion order, and commits share the subtrees that did not change.

The same content has different IDs in different repos. Blob deduplication also stays inside one repo.

### A core meant to be embedded

The core crate takes hashing, randomness, the clock and the device ID from the host. Request and response types can be exported as TypeScript and Swift definitions through features. The CLI is one host that calls this core.

## What it is not

- It is not git compatible. It cannot read a git repository and does not speak git's commands.
- It is not a service and has no server of its own. Devices exchange history through a remote: an append-only log plus a blob store, which is a shared directory or a program that speaks the messages of `luvcs-protocol`. The store is a SQLite file under `.luv/` in the work tree. A repo can also be exported and imported.
- It has no branches. There are no named references.
- It does not take "every file in the directory". The CLI only sees files that match a glob in `Luv.eure`. A file that matches nothing is never saved, listed or deleted.
- It is not only a line merger. Structured values are merged as structures. It does not, on the other hand, understand the meaning of text or the syntax of code when it merges.
- It is not an encryption or access control tool. A repo's ID key is stored in the store in plain form, and the store file is readable only by its owner. The remote's log carries that key too, so that a device that pulls can verify what it takes in; the remote has to be kept as private as the content, and encrypting it is the remote's job. An exported archive needs the same care.

## CLI

The `luv` command has only these for now:

```
luv init --include "**/*.md"
luv status
luv diff
luv commit -m "message"
luv log
luv restore [--commit cmt_...] [paths...]
luv remote set --path ../shared      # or --command "program args"
luv push
luv pull
```

`init` writes `Luv.eure`:

```
repo = "notes"
include = ["**/*.md"]
```

`remote set` writes `.luv/remote.eure`, next to the store. `Luv.eure`, the file that is meant to travel with the files, never names a command, and `.luv`, `.git` and `.jj` are never tracked or restored at any depth, so files that come from someone else's history cannot change the remote or make `luv` run a command. `push` sends what the remote has not seen and never conflicts. `pull` takes the remote's history in and leaves your files alone. A remote is trusted like a secret: the first time a repo arrives from a remote, luv accepts the ID key that remote gives it, so pull only from remotes you trust. A second device starts with `remote set`, `pull`, then `init --repo <name>`, which attaches to the repo that was pulled, and `restore`.

Undo / redo, integrate and conflict resolution exist in the request API but not yet in the CLI. After concurrent commits a repo has several heads and `luv log` shows them all; merging them from the CLI is not there yet. Symbolic links are not tracked; luv prints a warning and skips them.

## Layout

| Crate | What it is |
|---|---|
| `luvcs` | The core: objects, IDs, tree, merge, local store, request API, remotes, push and pull |
| `luvcs-cli` | The `luv` command |
| `luvcs-macros` | Attribute macro that generates the FFI-facing types |
| `luvcs-protocol` | The messages between luvcs and a remote: log entries, blob chunks, remote configuration |

## Embedding and the remote contract

Push follows the remote's advertised entry, blob chunk, and append limits.
Remotes that omit limits use defaults of 1 MiB, 4 MiB, and 256 entries.
A single object exceeding the entry limit is rejected with `too_large` before
any remote write. Use `SaveBinary` / `PutBlob` for large binary content.

`RemoteError::is_retryable()` and `SyncError::is_retryable()` identify
`locked` and `unavailable` as retryable errors. The host controls the delay
and retry count; other errors require user attention.

Use `ListObjects` / `ReadObject` with a commit ID to read a snapshot,
`Log` / `Show` for commit history, and `ReadBlob` for binary content.
Object views include text, record JSON, and conflict terms.

The default `bundled-sqlite` feature includes SQLite. Hosts can disable
default features and select `store` and `rust-digest` as needed.
`DirRemote` and `ProcessRemote` are native-only; `Remote`, `MemoryRemote`,
and the protocol types are available on wasm.

## License

Dual-licensed under MIT or Apache-2.0, at your option.

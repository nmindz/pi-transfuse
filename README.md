# pi-transfuse

Converts coding-agent sessions between **DeepSeek Harness** (DSH) and **Oh My Pi** (OMP / pi), in both directions, and imports **Claude Code** sessions into Pi, with a Ratatui browser for picking what to move. It also archives sessions nobody has used in a while, either compressed in place or moved away to a backup store.

```
pi-transfuse                        # browse and convert interactively
pi-transfuse convert-all dsh        # migrate every DSH session into ~/.omp/agent/sessions
pi-transfuse convert-all claude     # import every Claude Code session Pi does not already hold
pi-transfuse verify                 # prove the conversions lose nothing
pi-transfuse stale                  # sessions unused for 30 days and the space they hold
pi-transfuse compact pack           # compress old sessions in place
pi-transfuse vault push /Volumes/Backup/pi-vault   # move old sessions to a backup store
pi-transfuse migrate pull devbox --delete          # bring OMP sessions over from another machine
```

## Why it exists

The harnesses persist sessions differently and none reads the others:

| | DSH | Pi | Claude Code |
|---|---|---|---|
| Location | `~/.dsh/sessions/<bucket>/<id>/session.v<N>.jsonl.zstd` | `~/.omp/agent/sessions/<bucket>/<ts>_<id>.jsonl` | `~/.claude/projects/<bucket>/<id>.jsonl` |
| Framing | zstd-compressed JSONL, one file per released generation | plain JSONL, 256-byte fixed-width title slot first | plain JSONL, one record per content block |
| Model | flat event log (`seq`, `time`, `type`, `data`) | append-only entry tree (`id`, `parentId`) | record tree (`uuid`, `parentUuid`) |
| Project bucket | `--Users-me-Projects-x--` | `-Projects-x` under home, `-tmp-x` under the temp root (`$TMPDIR`, else `/tmp`), `--opt-work-x--` elsewhere, all on symlink-resolved paths | `-Users-me-Projects-x`, every non-alphanumeric character as `-` |

Claude Code is a one-way source: its sessions import into Pi, and nothing converts into Claude Code.

## Install

Needs a Rust toolchain.

```sh
make install
```

Builds release and installs to `~/.local/bin` (override with `INSTALL_DIR=`). `make help` lists every target; `make ci` runs fmt, clippy, tests, and the release build.

Session roots are discovered from `~/.dsh`, `~/.omp`, and `~/.claude`, overridable with `DSH_HOME`, `OMP_HOME`, `CLAUDE_CONFIG_DIR`, or the direct `DSH_SESSIONS_DIR`, `OMP_SESSIONS_DIR`, `CLAUDE_SESSIONS_DIR`.

## Browser

`pi-transfuse` with no subcommand opens the browser.

| Key | Action |
|---|---|
| `tab`, `1`, `2`, `3`, `4` | switch between the DSH, Pi, Claude Code, and [Remote](#remote-tab) tabs |
| `j` `k` `↑` `↓`, `g` `G`, `PgUp` `PgDn` | move |
| `space` | toggle selection; on a Claude Code project, every shown session in it |
| `a` / `A` | select everything shown / clear the selection |
| `/` | filter by id, title, or cwd |
| `enter` | preview the transcript (`j`/`k` scroll, `space` pages, `esc` closes); on a project, fold it |
| `←` `→` / `h` `l` | fold / unfold a Claude Code project |
| `x` | convert the selection, or the row under the cursor; on Claude Code, import the checked sessions after a confirmation |
| `m` | write `./exports/<id>.md` |
| `d` | toggle dry-run |
| `f` | allow overwriting an existing destination |
| `e` | show sessions that hold no conversation (hidden by default) |
| `r` | rescan every root |
| `?` / `q` | keys / quit |

The detail pane shows where a session would land and warns when that file already exists.

### Remote tab

The fourth tab runs [`migrate`](#migrating-omp-sessions-between-hosts) against another host, always OMP to OMP; the converter's `converts to DSH` in the Pi tab is a different thing, between harnesses on this host. `C` (or `enter` while disconnected) asks for a host, offering every concrete `Host` of `~/.ssh/config` as you type, and `P` switches between pulling from it and pushing to it. The sending side's OMP sessions, live and packed in place, then fill the same [picker](#choosing-sessions) `compact` uses, with its checkboxes, `space`, `a` (every shown session, or none when all are), `A` (clear), `p` (a whole path), `/` search, `f` filters, `[`/`]` threshold, `U` untitled and sort keys, and with `D` delete-source and `R` relocate-to in its top bar. `x` or `enter` asks for confirmation and migrates the selection, with transfer progress on the line above the list and one line per session in the activity log, after which the list is read again with your filters, sort and selection kept; `r` reads it again by hand. Sessions open in a running omp on the sending side, or already on the receiving side live or packed, are dimmed.

When ssh needs a password, a key passphrase or a decision on an unknown host key, a dialog shows its question whichever tab is open: typing stays hidden for passwords and passphrases, `enter` answers and `esc` declines. The browser starts ssh detached from the terminal with itself as `SSH_ASKPASS`, so ssh's prompts and messages never draw over the screen; its messages land in the activity log. The tab uses plain `ssh` with `~/.ssh/config` for everything else and finds pi-transfuse on the remote host [as `migrate` does](#migrating-omp-sessions-between-hosts); `PI_TRANSFUSE_REMOTE_BIN` names it when that fails.

### Claude Code tab

Sessions are grouped under their working directory. Projects and sessions each carry a checkbox: `[x]` checked, `[ ]` unchecked, and `[-]` on a project whose sessions are partly checked. A session shows its name — a `/rename`, an agent name, or Claude Code's generated title, in that order — and, when it has none, its first typed prompt in italics, or its id.

Each session is judged against what Pi already holds (see [Duplicates](#duplicates)):

| Mark | Meaning | Starts |
|---|---|---|
| `=` | Pi already holds it | unchecked |
| `≈` | some of its prompts are in one Pi session | checked |
| none | nothing in Pi resembles it | checked |

Uncheck the projects or sessions you already moved some other way, then `x` asks for confirmation and imports what remains checked. Imported sessions turn `=` and drop out of the selection.

## Commands

```sh
pi-transfuse list [dsh|pi|claude] [--filter TEXT]

pi-transfuse convert <id-or-path> [--to dsh|pi] [--out DIR] [--dry-run] [--force] [--include-empty] [--include-duplicates]
pi-transfuse convert-all <dsh|pi|claude> [--filter TEXT] [--out DIR] [--dry-run] [--force] [--include-empty] [--include-duplicates]

pi-transfuse prune-empty [--filter TEXT] [--execute]
pi-transfuse verify [dsh|pi|claude] [--filter TEXT] [--all]
pi-transfuse export <id-or-path> [--format md|text] [-o FILE]
pi-transfuse relocate <OLD_DIR> <NEW_DIR> [--dry-run] [--wait] [--move-dir]

pi-transfuse stale [--older-than 30d] [--include|--exclude FIELD=VALUES]... [--filter TEXT] [--sort KEY] [--reverse] [--json]
pi-transfuse compact list|pack|unpack|offload …
pi-transfuse vault init|info|list|push|pull|reindex <STORE> …
pi-transfuse migrate list|pull|push <HOST> …
```

`<id-or-path>` accepts a session-id prefix or a path to a session file; the direction is inferred from the source. Without `--out`, conversions write into the destination harness's real session root, so the result is immediately listed and resumable there. Claude Code sessions always land in Pi, and `--include-duplicates` imports one even when Pi already holds a copy.

### Empty sessions

A cancelled or never-used launch leaves behind a log holding nothing but startup records — permission preset, sandbox mode, approval policy — and no prompt or reply. Importing those buries the real sessions behind rows of `(no messages)`.

Conversion therefore **skips any session with no conversation message** unless `--include-empty` is passed, and the browser hides them until `e` reveals them. `pi-transfuse prune-empty` cleans up a corpus imported before that rule existed: it reports the empty sessions this tool wrote and deletes them under `--execute`. Native sessions of the destination harness are never candidates, empty or not.

A Claude Code session counts as empty when no model ever replied — it ran only slash commands such as `/model` or `/login`, or its prompts went unanswered — and the same rule applies to each of its subagents. Assistant records Claude Code writes itself, such as API errors, are not replies.

### Relocating Pi sessions

`pi-transfuse relocate OLD NEW` moves every Pi session whose header `cwd` is `OLD` or lies under it (on a path-component boundary, so `OLD-other` never matches) to the bucket of the matching path under `NEW`, so `omp --resume` and `omp -c` in `NEW` find them without a re-root prompt. Each session moves as a unit — transcript, artifact directory, lock sidecar — with its header `cwd` and `additionalDirectories` remapped and the old path appended to `previousSessionFiles`; subagent transcripts get the same cwd rewrite. The title slot and every entry stay byte-identical. A failed step rolls the whole session back. Moves rename within a filesystem and otherwise copy, verify size and SHA-256, then delete.

Sessions a running omp holds (`omp session lease`) are reported `held` and skipped; `--wait` polls until they are released. An existing destination is a `conflict`. Terminal breadcrumbs, custom session markers, `history.db` and `composer.db` are updated for moved sessions only (`sqlite3` must be on `PATH`). `--move-dir` renames `OLD` to `NEW` first and refuses a non-empty destination or a cross-filesystem rename; without it, `NEW` must exist. `--dry-run` prints the plan and writes nothing. Exit status: 0 when everything moved, 1 when any session was held, conflicting or failed, 2 on argument or environment errors. Re-running the same command finishes what is left.

### Migrating OMP sessions between hosts

`migrate` moves OMP sessions between this machine and another over SSH: `pull` brings them here, `push` sends them there. Each session goes over as one archive in the [archive format](#archive-format) (zstd by default), is checked on arrival against the SHA-256 its sender computed and against the per-file sums inside, and is restored under the receiving host's Pi root into the bucket of its cwd, ready for `omp --resume` there.

```sh
pi-transfuse migrate list [HOST] [FILTERS] [--sort KEY] [--reverse] [--older-than AGE] [--long | --json] [SSH]
pi-transfuse migrate pull [HOST] [SESSION...] [FILTERS] [--sort KEY] [--reverse] [--older-than AGE] [--delete] [--relocate DIR] [--algo gzip|zstd] [--level N] [--threads N] [--yes] [--dry-run] [SSH]
pi-transfuse migrate push [HOST] [SESSION...] [FILTERS] [--sort KEY] [--reverse] [--older-than AGE] [--delete] [--relocate DIR] [--algo gzip|zstd] [--level N] [--threads N] [--yes] [--dry-run] [SSH]

SSH: [--ssh PROGRAM] [--ssh-arg ARG]... [--remote-bin PATH]
```

`<HOST>` is anything `ssh` accepts as a destination: `user@host`, or a `Host` from `~/.ssh/config`. Without a host, `migrate list`, `pull` and `push` open the browser on its [Remote tab](#remote-tab), asking for one, with `pull` or `push` preset; that needs a terminal. The remote host needs the same pi-transfuse version; `migrate` starts `pi-transfuse migrate serve` there through `ssh -T` and speaks to it over that connection, so nothing listens on a port. `--ssh` picks another client, and each `--ssh-arg` is passed to it before the host (`--ssh-arg=-p --ssh-arg=2222`). Only OMP sessions migrate; `--include`/`--exclude harness=…` that leaves OMP out is an error.

sshd runs the remote command through your login shell without a terminal, so zsh, bash and fish skip the rc files where `PATH` usually grows (`~/.zshrc`, `~/.bashrc`, `config.fish`). `migrate` therefore asks that shell, as an interactive login shell (`$SHELL -lic`, its stdin closed), for its `PATH`, and runs pi-transfuse with it; the same `PATH` lets the remote side find `omp`. When pi-transfuse is not on it, the path this host runs it from is tried there (with your home swapped for the remote one), then `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin` and `/usr/local/bin`. `--remote-bin` (or `PI_TRANSFUSE_REMOTE_BIN`) names it outright, as a path (`~/` is the remote home) or a command name looked up on that `PATH`. Anything an rc file prints is discarded, and rc output never reaches the migration stream. If none of it finds pi-transfuse, the remote side says so and the run exits 2.

`--delete` removes each session from the host it came from, only after the other host restored it with every checksum passing, and only when none of its files changed since it was packed and a running omp has not opened it (checked through `omp session lease` when `omp` is on that host's `PATH`); otherwise the session stays at its source and is reported as an `error` with the reason. Without `--delete` the source keeps its copy.

`--relocate DIR` is a built-in `relocate` for the arriving sessions: each is restored as if it had run in `DIR` on the receiving host, so its header `cwd` becomes `DIR`, `additionalDirectories` and subagent cwds under the old cwd move beneath `DIR`, and it lands in `DIR`'s bucket. `DIR` must exist on the receiving host, and `~` there is that host's home. Every other byte of the session stays as it was.

Without session arguments `pull` and `push` open the [picker](#choosing-sessions) over the sending host's OMP sessions, where `D` toggles delete-source and `R` edits the relocation directory; both show in the top bar and in the confirmation. The [filters](#sorting-and-filtering) work as for `compact`: `--include project=api,web` keeps those projects, `--include path=~/Projects/_myself` keeps every session under that directory, `--hide-untitled` drops sessions without a title, `--older-than` sets the threshold (none by default), and `--yes` takes every match. A `path` filter names directories of the sending host: on `pull` and `list`, `~` and relative paths mean the remote user's home. Sessions a running omp holds on the sending host, and sessions the receiving host already has, live or packed, are dimmed and cannot be selected; named or taken under `--yes`, they are reported `held` or `conflict`. `--dry-run` prints the plan without packing anything, and `migrate list` shows the remote host's OMP sessions with which are packed in place, open there, or already here. The browser's [Remote tab](#remote-tab) does the same interactively.

Sessions packed in place (`compact pack`) migrate too, and stay packed: the archive is checked end to end, sent as it is, checked again on arrival, and put in the bucket of its cwd, byte for byte. With `--relocate`, or when the cwd maps to another bucket on the receiving host, it is unpacked in the hidden staging directory, moved and rewritten, and packed again with the compression it came in. `--delete` removes the sender's archive once it arrived, unless the archive changed meanwhile.

Authentication is OpenSSH's own: keys, ssh-agent and `~/.ssh/config` apply exactly as for `ssh <HOST>`, so a key per host is an `IdentityFile` under its `Host` entry (or `--ssh-arg=-i --ssh-arg=KEY` for one run). On the command line ssh asks for a password, a key passphrase or a new host key in the terminal, before the picker opens; without a terminal only key or agent authentication works, and `--ssh-arg=-oBatchMode=yes` makes a missing key fail at once instead of waiting.

A broken connection stops the run with exit 2. The session in flight stays at its source, and a restore that did not finish leaves no partial session on the receiving host: archives arrive and unpack in a hidden `.pi-transfuse-migrate-*` directory beside the Pi root, removed when the run ends. Otherwise the exit status is that of `compact` and `vault`.

## Archiving old sessions

Sessions and their artifact directories pile up. `stale` finds the ones nobody has touched in a while, and two command families get them out of the way:

| | `compact` | `vault` |
|---|---|---|
| Archive goes | into the session's own bucket, beside where it lived | into a store folder you name: external disk, NAS mount, synced drive |
| Local disk keeps | the compressed archive | nothing |
| Brought back by | `compact unpack` | `vault pull <STORE>` |

`compact offload <DEST>` bridges the two: it moves everything already packed in place into a store at `<DEST>`, so packing in place first and offloading later ends in the same store a `vault push` would have filled.

```sh
pi-transfuse stale [--older-than 30d] [FILTERS] [--sort KEY] [--reverse] [--json]

pi-transfuse compact list   [FILTERS] [--sort KEY] [--reverse] [--long | --json]
pi-transfuse compact pack   [SESSION...] [--older-than 30d] [FILTERS] [--sort KEY] [--reverse] [--algo gzip|zstd] [--level N] [--threads N] [--yes] [--dry-run]
pi-transfuse compact unpack [SESSION|ARCHIVE...] [FILTERS] [--sort KEY] [--reverse] [--keep] [--yes] [--dry-run]
pi-transfuse compact offload <DEST> [SESSION|ARCHIVE...] [FILTERS] [--sort KEY] [--reverse] [--pick] [--name NAME] [--keep] [--dry-run]

pi-transfuse vault init    <STORE> [--name NAME]
pi-transfuse vault info    <STORE>
pi-transfuse vault list    <STORE> [FILTERS] [--sort KEY] [--reverse] [--long | --json]
pi-transfuse vault push    <STORE> [SESSION...] [--older-than 30d] [FILTERS] [--sort KEY] [--reverse] [--algo gzip|zstd] [--level N] [--threads N] [--keep] [--yes] [--dry-run]
pi-transfuse vault pull    <STORE> [SESSION|ARCHIVE...] [FILTERS] [--sort KEY] [--reverse] [--remove] [--yes] [--dry-run]
pi-transfuse vault reindex <STORE>

FILTERS: [--include FIELD=VALUE[,VALUE…]]... [--exclude FIELD=VALUE[,VALUE…]]... [--filter TEXT] [--hide-untitled]
```

A session is everything it owns: a Pi transcript with its artifact directory (subagent transcripts included) and lock sidecar; a DSH session directory with every log generation; a Claude Code transcript with its subagent directory. Its last use is the newest modification time among those paths. `--older-than` takes `30d`, `2w`, `36h`, or a bare number of days. `stale` lists the sessions past the threshold with their sizes and a per-path total, and changes nothing.

### Sorting and filtering

A session's **path** is the directory it ran in; its **project** is that directory's name, so `~/work/api` and `~/old/api` are two paths of one project.

`--sort` orders listings, the picker's starting order, and the order `--yes` works through. Each key has a natural direction, and `--reverse` flips it:

| Key | Order | Picker key |
|---|---|---|
| `used` | last use, oldest first (default for `compact` and `vault push`/`pull`) | `u` |
| `size` | size before archiving, largest first (default for `stale`) | `s` |
| `archive` | archive size, largest first; live sessions last | `z` |
| `created` | creation time, oldest first | `c` |
| `path` | path, A→Z (default for `vault list`) | `d` |
| `project` | project name, A→Z, then path | `n` |
| `harness` | harness | `h` |
| `title` | title, A→Z | `t` |

`--include FIELD=VALUE[,VALUE…]` keeps sessions matching any of the values; `--exclude` drops them. Both repeat, and a session must match every `--include`. Fields: `harness` (`pi` or `omp`, `dsh`, `claude` or `claude-code`), `project` (folder name, any case), and `path` (that directory and everything below it, on whole path components; `~` expands, to the remote home for `migrate pull` and `migrate list`). Excluded harnesses are not read at all. `--filter TEXT` keeps sessions whose id, title, or path contains the text, and `--hide-untitled` leaves out sessions without a title.

```sh
pi-transfuse stale --exclude harness=dsh --sort project
pi-transfuse vault push /Volumes/Backup/pi-vault --include harness=claude-code,omp --exclude path=~/Projects/active
pi-transfuse compact list --include project=veritas --sort archive --reverse
```

Exit status of `compact` and `vault`: 0 when every chosen session went through, 1 when any was `held`, a `conflict`, `changed` while packing, or an `error`, 2 on argument or environment errors.

### Choosing sessions

Without session arguments, `pack`, `unpack`, `push`, `pull`, `migrate pull` and `migrate push` open a picker on a terminal:

| Key | Action |
|---|---|
| `space` | toggle the session under the cursor |
| `a` / `A` | select every shown session, or deselect them when all are selected (the header checkbox shows `[ ]`, `[-]`, `[x]`) / clear the whole selection |
| `p` | toggle every shown session in the cursor's path |
| `/` | filter by id, title, or path |
| `f` | filter panel over each harness, project, directory and the untitled sessions, counted over what the search and threshold leave (see below) |
| `U` | hide or show sessions without a title |
| `[` / `]` | lower / raise the unused-for threshold through 0, 7, 15, 30, 60, 90, 180, 365 days |
| `u` `s` `z` `c` `d` `n` `h` `t` | sort by last used, size, archive size, created, path, project, harness, title; the same key again reverses |
| `enter` | confirm the selection, or the session under the cursor |
| `?` / `q` | keys / cancel |

The top bar shows the threshold, the order, and what the filters hide; the detail pane shows everything known about the session under the cursor. Hidden sessions are never acted on, even when selected. A session that cannot be restored because a live copy exists is dimmed and cannot be selected. Naming sessions (an id, an id prefix, or an archive path) skips the picker and ignores the threshold, `--yes` takes every match past the threshold, and `--dry-run` prints the plan; off a terminal one of them is required.

The filter panel lists harnesses, projects, the directories that group several session paths (`~/Projects/_myself` when more than one project sits there; one holding every session is left out, since it filters nothing), and the untitled sessions. Typing narrows the panel by fuzzy match: a substring anywhere ranks first, earliest first, then the letters in order (`~/proj/w` finds `~/Projects/work`); a directory matches on its `~` form or its full path. `↑` `↓` move, `space` shows or hides the line under the cursor (a hidden directory hides everything below it), `Ctrl-O` shows only that harness, project or directory, `Ctrl-S` shows only the lines the search matches among the cursor's kind (several projects at once), `Ctrl-A` shows everything, `Ctrl-U` clears the search, and `esc` clears it, then closes. In the Remote tab, `~` is the sending host's home.

### Compression

`--algo gzip` (the default; levels 0–9, default 6) writes `.tar.gz` that any `tar` opens. `--algo zstd` (levels −7 to 22, default 3) writes `.tar.zst`, compresses on every core (`--threads N` sets how many, `0` keeps it on one), and is faster and smaller at a comparable level; 19 and up are slow and the smallest. Higher levels cost time and CPU only when packing; unpacking is fast at any level. On the sessions measured here, a 16.2 MB Pi session packed to 1.6 MB at zstd 19 and to 4.1 MB at gzip 9. DSH logs are zstd-compressed already, so compacting them saves nothing; pushing them to a vault still frees all of it.

### Archive format

An archive is a tar stream of the session's paths relative to its harness root, opening with `.pi-transfuse/<name>.manifest.json` and closing with `.pi-transfuse/<name>.sha256.json`. The manifest records the harness, id, title, project, bucket, member paths, creation and last-use times, file count and size, subagents, message count when known, compression, and the harness root and machine (host, user, OS, architecture, tool version) it was packed on. The trailer holds the SHA-256 of every file.

Packing writes the archive under a hidden temporary name, reads it back end to end against the sums, checks that the session did not change meanwhile (`changed` otherwise), and only then names the archive and removes the session. Pi sessions a running omp holds (`omp session lease`) are skipped as `held`; for DSH and Claude Code sessions the unchanged-files check is the guard. Restoring writes into a staging directory inside the bucket, checks every file against the sums, refuses any entry outside the session's paths or of a kind other than file and directory, never overwrites a session that exists (`conflict`), and puts back contents, permissions, and modification times. By hand: `tar -xf ARCHIVE -C ROOT --exclude .pi-transfuse`.

### Compact

`compact pack` replaces each chosen session with `<bucket>/<name>.tar.gz` (or `.tar.zst`) in the bucket it lived in. A packed session drops out of `omp --resume`, the browser, and the other commands until `compact unpack` restores it to the same paths and deletes the archive (`--keep` retains it). `compact list` shows every session packed in place; `--long` prints everything its manifest records, `--json` all of it for scripts.

`compact offload <DEST>` moves every session packed in place, or those the filters, session names, or `--pick` choose, into a store at `<DEST>`: an external disk, a NAS mount, a synced drive. A folder that is not a store yet becomes one (`--name` names it, the folder name by default), and a missing `<DEST>` is created when its parent exists; a missing parent is refused as an unmounted volume. It checks the destination has room for all of it before copying anything. Each archive is copied as it is, verified against its SHA-256, flushed to disk, and only then deleted locally (`--keep` leaves it), and a bucket left empty goes with it. Running it again moves only what was packed since; content the store already holds is not stored twice. `vault pull <DEST>` restores the sessions to the paths they were packed from, and `vault info` and `vault list` describe what is there.

### Vault

A store is any existing folder after `vault init`: a USB disk, a NAS mount, a Google Drive or Dropbox folder. Every store stands on its own:

```
<STORE>/
  pi-transfuse-store.json                                  identity, history, every archive's manifest
  archives/<harness>/<bucket>/<name>.<packed-at>.tar.gz    one per pushed session
```

The index records the store's name and id, its creation and last update with the machine behind each, and for every archive its manifest, size and SHA-256, when, from which machine and from which harness root it was pushed, and each restore since. `vault info` summarizes a store by harness, origin machine, and path; `vault list` lists its sessions (`--long` for everything, `--json` for scripts).

`vault push` moves sessions into the store: live sessions pack straight into it, sessions already packed in place are copied as they are, and the local copy is removed once the store's copy checks out (`--keep` leaves it). Content the store already holds for that session is not stored twice. `vault pull` restores sessions to the paths they were packed from, under the current machine's harness root, and keeps the store's copy (`--remove` deletes it); naming a session pulls its newest copy. A session pushed, pulled, continued and pushed again keeps both copies, stamped with their pack times.

The index caches what the archives themselves record and is written in batches during a push. `vault reindex` verifies every archive and rebuilds the index from them, keeping the push and restore history of archives it already knew and recovering the store's identity from the previous index generation when the current one is lost. Commands that change a store lock it against a concurrent run where the filesystem supports locks.

Claude Code keeps more per-session state outside its projects directory (`file-history`, `todos`, `session-env`); archives hold the transcript and its subagents only.

## Mapping

| DSH event | Pi entry |
|---|---|
| `session` header | `session` header + title slot |
| `session/title` | `title_change`, and the title slot |
| `model/selection` | `model_change` |
| `system/message` (first) | `session_init` |
| `developer/message` | `message` role `developer` |
| `user/message` source `user` | `message` role `user` |
| `user/message` other sources | `custom_message` |
| `assistant/message` | `message` role `assistant` (`reasoning`→`thinking`, `tool-call`→`toolCall`) |
| `tool/call` | `custom` `tool_execution_start` |
| `tool/result` | `message` role `toolResult`, `meta`→`details` |
| `compaction/summary` | `compaction` |
| everything else | `custom` `ai.deepseek.dsh.<type>` holding the row verbatim |

DSH generations v2, v3, and v4 all read; the newest generation present in a session directory is the authoritative one. Pre-v4 tool results nest their output inside a `tool-result` block and v4 stores it flat — both shapes convert and restore correctly.

Pi-only entries (`thinking_level_change`, `label`, `reset_boundary`, extension records) become DSH events typed `pi/<entry>` carrying the envelope's `ignorable: true` marker, which DSH retains without refusing the log.

## Claude Code import

| Claude Code record | Pi entry |
|---|---|
| `user` prompt (text, images) | `message` role `user` |
| `user` `tool_result` block | `message` role `toolResult` |
| `user` with `isMeta` (hook feedback, skill bodies, command caveats) | `custom_message` `claude-code.meta`, hidden but in context |
| `user` with `isCompactSummary` | `compaction`, sized from the `compact_boundary` before it |
| `assistant` records of one response | one `message` role `assistant` (`thinking` keeps its signature, `tool_use`→`toolCall`), after a `model_change` when the model changes |
| `queued_command` attachment | `message` role `user` — a prompt typed while the model was busy has no other copy |
| `custom-title`, `agent-name`, `ai-title`, `summary` | title slot with its source, and one `title_change` |
| subagent `<id>/subagents/**/agent-<x>.jsonl` | Pi subagent file beside the session, titled by its `meta.json` description |

Hook output, context attachments, file-history snapshots, permission and mode switches, and cost state are Claude Code runtime bookkeeping and are left out.

Claude Code writes each content block of a response as its own record and starts tools before the response ends, so blocks arrive interleaved with tool results and injected prompts. The import merges a response into one assistant message, puts its tool results directly after it, and holds injected and queued prompts until the results are in, which is the order a provider accepts on resume. Entries chain in file order; only a turn that starts from an earlier point — a rewind, or a prompt typed while a tool ran — opens a Pi branch there, and Pi's active path matches the one Claude Code continued on.

Each import gets a fresh, deterministic session id, so it never collides with its source and re-importing lands on the same file. A `foreign_session_import` record names the source session, the same record OMP's own `--from-claude` writes.

### Duplicates

A Claude Code session can already be in Pi under another id: imported before, imported by `omp --from-claude`, or migrated through DSH. Two signals survive those trips:

- a `foreign_session_import` record naming the Claude Code session marks it **already imported**;
- otherwise the human prompts are fingerprinted — whitespace-normalized, at least 16 characters, command envelopes excluded — and a Pi session holding **every** fingerprinted prompt marks it a duplicate. Holding some marks it `≈` with the share, and the decision stays yours.

`convert-all claude` and `convert claude` skip duplicates unless `--include-duplicates` is passed; `list claude` shows each verdict. A copy that rewrote the prompts themselves is not detected — the browser's project checkboxes are the control for that.

## Fidelity

Every converted entry carries a `dsh` field holding the source `seq`, event type, surface intent, and the source payload **minus exactly the fields the Pi entry already materializes** — message content, tool-result metadata, compaction summary, title. Nothing is stored twice, and converting back rebuilds the original rows instead of an approximation.

`pi-transfuse verify` proves this against your real sessions:

- **DSH → Pi → DSH** must reproduce the header and every event row byte for byte.
- **Pi → DSH → Pi** must reproduce the header, the title, and every transcript block. Entry counts legitimately grow here, because DSH replay needs the turn and step scaffolding Pi has no entries for; that scaffolding is dropped again on the way back.
- **Claude Code → Pi**, being one-way, must carry every prompt, reply, reasoning block, tool call, tool result, injected context message, and compaction of the source and of each subagent. Each kind is compared in its own order, since a response's tool results legitimately move up next to it.

```
$ pi-transfuse verify dsh
553 checked · 553 identical · 0 changed · 0 unreadable

$ pi-transfuse verify claude
68 checked · 68 identical · 0 changed · 0 unreadable
```

The 68 Claude Code sessions measured here import to 567 Pi files (subagents included) in about 4 s, and every file loads in OMP's own session manager with each tool call answered on the active path.

### Known limits

- Pi files are uncompressed, so a migrated corpus is roughly 4–5× the size of its zstd originals.
- Conversion does not copy attachments. DSH keeps binary attachments in `~/.dsh/attachments`; references survive in the log, the blobs are not moved.
- DSH `assistant/attempt` streaming telemetry has no Pi counterpart and rides along as an `ai.deepseek.dsh.assistant/attempt` record, invisible in the Pi transcript.
- Sub-sessions are independent sessions on every side; each converts on its own and the parent link is carried through the header.
- Claude Code keeps transcripts for `cleanupPeriodDays` (30 by default). A session migrated before its transcript was cleaned up has nothing left to compare against; only its project checkbox can keep it out.
- Claude Code's file-history checkpoints, the snapshots behind its rewind of file edits, do not import; the conversation does.
- Pi entry ids are minted deterministically from the source session and sequence, so re-running a conversion is byte-identical — but a native Pi session converted to DSH and back gets new ids for its mapped messages.

## Layout

```
src/paths.rs          session roots and the cwd-bucket encodings
src/dsh.rs            DSH discovery, zstd JSONL decode/encode
src/pi.rs             Pi discovery, title slot, JSONL decode/encode
src/claude.rs         Claude Code discovery and JSONL decoding
src/convert.rs        both DSH ⇄ Pi directions
src/claude_import.rs  Claude Code → Pi import
src/dedup.rs          duplicate detection for Claude Code imports
src/transcript.rs     readable projection shared by the preview and Markdown export
src/tui.rs            Ratatui browser
src/units.rs          sessions as on-disk units and their age; choosing and reporting shared by stale, compact, vault
src/archive.rs        session archives: manifest, tar, gzip/zstd, read-back verification, staged restore
src/compact.rs        compaction in place
src/vault.rs          backup stores
src/pick.rs           Ratatui multi-select picker
src/facts.rs          what sessions sort and filter by: sort keys, --include/--exclude rules
src/main.rs           CLI
```

```sh
cargo test   # format, mapping, and round-trip unit tests
```

## License

GPL-3.0-only. See [LICENSE](LICENSE).

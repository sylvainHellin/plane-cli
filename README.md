# plane

A small CLI for a self-hosted Plane CE instance: read and write issues without a shell pipeline reaching for `curl`, `jq`, or a Python one-liner.

## Why it exists

Two problems, one binary.

Driving Plane CE over `curl` means rediscovering the same endpoint quirks every session: which paths need a trailing slash, which field the API actually writes, how the cursor paginates.
And the shell workarounds around them are fragile: `python3 -c` to reshape JSON, `PAT=$(pass-cli ...)` command substitution, a token staged in a temporary file.
The CLI encodes the quirks once, reads the token itself so it never passes through the shell, and answers in a form that pipes straight into `jq`.

## Install

```
cargo install --path .
```

## Configuration

```
plane config set workspace acme
plane config show
```

Settings live in `~/.config/plane/config.toml` (`$XDG_CONFIG_HOME/plane/config.toml` when that variable is set), written with `0600` permissions.
A file rather than exported variables, because every context that runs `plane` reads the same file: an interactive shell, a systemd user unit, an editor, an agent, a cron job.
An export only reaches the processes that descend from the shell that ran it.

| Key | Variable that overrides it | Default |
|---|---|---|
| `workspace` | `PLANE_WORKSPACE` | none; required |
| `api_base` | `PLANE_API_BASE` | `http://localhost:8090/api/v1` |
| `web_base` | `PLANE_WEB_BASE` | none; issue URLs are omitted when unset |
| `pass_vault` | `PLANE_PASS_VAULT` | `Personal` |
| `pass_item` | `PLANE_PASS_ITEM` | `plane` |
| `pass_field` | `PLANE_PASS_FIELD` | `PAT` |

Precedence per setting is variable, then file, then default, so `PLANE_API_BASE=... plane project list` still points one call at another instance without touching the file.
`plane config show` prints the effective value of every setting next to the source it came from (`env`, `file`, `default`), because a variable shadowing the file is otherwise indistinguishable from a file that was never written.

```
plane config set <key> <value>
plane config unset <key>          # falls back to the default
plane config show
plane config path
```

An unknown key is an error listing the valid ones, rather than a line quietly stored in the file and never read.

`workspace` is the workspace slug, which is the path segment the web UI puts before `/browse/`.
Every API path is scoped by it, so there is no sensible default and an unset one is an error rather than a guess.

`web_base` is the browser origin, which is not derivable from the API base when a reverse proxy serves them differently.
Without it the commands work and simply print no `url:` line, which is better than printing a link that goes nowhere.

## Auth

`PLANE_API_KEY` if set, otherwise the token is read from a Proton Pass entry through [`pass-cli`](https://proton.me/pass).
Which entry is configurable through `pass_vault` / `pass_item` / `pass_field`, so nothing about one person's vault layout is baked into the binary.

The token itself is not a setting and `plane config set` refuses to store one: a key name that reads as a credential (`api_key`, `token`, `pat`, ...) is rejected with that reason.
It is never printed, logged, or written to disk.

## Commands

```
plane issue get RES-12
plane issue list RES [--state todo] [--module "Paper 2 Drawings"] [--label deep]
plane issue create RES "title" [--module M] [--state S] [--priority P] [--due YYYY-MM-DD] [--label L ...] [--desc-md -]
plane issue create --from-note <note.md> "title" [...]
plane issue update RES-12 [--state|--priority|--due|--title|--module|--label]
plane issue comment RES-12 "text"
plane issue attach RES-12 <file> [<file> ...]
plane issue attachments RES-12
plane project list
plane module list RES
plane state list RES
plane label list RES
plane config set|unset|show|path
```

`--json` is global, so `plane issue get RES-12 --json | jq -r .name` works on every command.
Single-object commands print the API body as it came back.
List commands print `{count, results}` with every page merged into `results`, because a list is fetched across as many pages as the cursor takes and no single raw body exists to print.

Closing a ticket is `plane issue update RES-12 --state done`.
There is no `close` subcommand: it would be pure shorthand for that one flag, and the GUI gesture it mirrors is also just setting the status.

State, module, label, and priority names are matched case- and separator-insensitively, so `--state done`, `--state "in progress"`, and `--state in-progress` all land.
A name that matches nothing is an error listing the real options, never a filter that quietly returns zero rows.

### Labels

`--label` is repeatable on `create` and `update`, and takes label names rather than UUIDs.
On `list` it is single-valued, matching `--state`: one label filters usefully, and two would have to pick between AND and OR semantics.

```
plane issue update RES-12 --label waiting --label deep
```

On `update` it **replaces** the issue's label set rather than adding to it, because that is what CE's `labels` field does: patching one label onto an issue carrying two leaves it with one.
So every label an issue should end up with goes into the same invocation, and dropping all labels is not expressible through this flag today.

Labels are per project, so the same four names exist once per board as four different UUIDs; `plane label list RES` prints the ones that project really has.

### Attachments

```
plane issue attach RES-12 ~/Downloads/plan.pdf ~/Downloads/scan.png
plane issue attachments RES-12
```

Uploading is three calls (ask for a presigned target, POST the file to it, confirm), and the CLI does all three per file.
The MIME type is inferred from the extension, falling back to `application/octet-stream`, which uploads correctly and only costs browser previewing.
The presigned POST goes to the object store rather than to the API, so it never carries the PAT.
An empty file is refused before anything is uploaded, because the presigned policy demands at least one byte.

Several files are several independent uploads, not one transaction: each success prints as it lands, and a failure on a later file names the ones already attached, so a retry should list only the files that are missing.
Re-running the whole list attaches the earlier files a second time.

If a confirm fails, the attachment exists but stays invisible in the web UI; `plane issue attachments` marks that row `(upload not confirmed)` rather than pretending the file is there.
The listing prints name, size, MIME type, and asset id.
There is no download URL column: the asset endpoint answers 401 to a PAT, since it authenticates by browser session.

### `--from-note`

Creates an issue from an Obsidian note's frontmatter:

```yaml
plane_project_id: 48284b59-0000-4000-8000-000000000000
plane_module_id: a6afc502-0000-4000-8000-000000000000
```

The vault-to-Plane mapping is asymmetric (a vault project is a Plane module, a vault area is a Plane project), but nothing here has to know that: every bridged note denormalizes both UUIDs, so this is two string reads and no traversal.
A note carrying no `plane_module_id` creates at project level rather than failing, since not every bridged note belongs to a module.

An explicit `--module` overrides the note's module.

## Endpoint facts baked in

Everything below was verified against a running CE instance and is encoded once in `src/client.rs`, so no caller has to remember it.

| Fact | Consequence |
|---|---|
| `GET /workspaces/<slug>/issues/<IDENT>-<n>/` returns 200 | `issue get` is one call, not a project scan |
| `?expand=state,labels` works and substitutes in place | read `.state.name`; there is no `.state_detail` on CE |
| trailing slash mandatory | `projects` 301s, `projects/` 200s |
| no `-lite` endpoints | `projects-lite/` 404s; the plain form is the one that exists |
| `/modules/<id>/module-issues/` returns issue objects directly | no join wrapper, no `issue_detail` |
| responses carry `next_page_results` | every list follows the cursor; nothing reads page 1 and stops |
| the default states are the same on every project | Backlog, Todo, In Progress, Done, Cancelled |
| `description_html` is the write field | markdown is converted before write, never passed through |
| an issue's module lives in a join table | `--module` is a second call, and the issue record keeps reporting `"module": null` |
| `labels` is the write field, and `label_ids` is accepted and ignored | `--label` writes `labels`; the plausible-looking spelling silently loses the labels |
| `labels` replaces the whole set on a PATCH | `--label` is documented as replacing, not appending |
| attachments live under `work-items/`, not `issues/` | the `issues/` spelling 404s, while issue CRUD and comments need `issues/` |
| the presigned upload host follows the API host | attaching works remotely, not only from the server itself |
| 60 requests per minute per PAT | a 429 says so instead of surfacing a bare status code |

## Using it against a remote instance

Nothing but the configuration changes when the instance is not on localhost, as long as the reverse proxy in front of it routes `/api/v1`:

```
git clone <this repo> && cd plane-cli
cargo install --path .

plane config set workspace acme
plane config set api_base https://plane.example.com/api/v1
plane config set web_base https://plane.example.com
export PLANE_API_KEY=<a personal access token>   # only if pass-cli is not set up here

plane project list
```

Auth resolves the same way everywhere: `PLANE_API_KEY` if set, else `pass-cli`.
A machine with a Proton Pass session set up needs no key variable at all.

Attachments work remotely too: the presigned upload URL is derived from the host the request arrived on, so a remote API base hands back a reachable upload target rather than a `localhost` one only the server could use.

## Not in v2

Cycles, pages, worklogs, initiatives.
Still raw-API territory: creating labels, projects, and modules, deleting anything, and reading comments back.

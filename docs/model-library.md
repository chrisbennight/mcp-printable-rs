# Model library

The optional `library` tool finds existing 3D designs on MakerWorld, Printables
and Thingiverse and saves chosen files into a project. The agent can then
import, inspect, modify or slice them with the other tools.

It is the only Printable feature that contacts the public internet. It is off
unless an operator enables at least one site.

## Enable sites

Set `PRINTABLE_LIBRARY_SITES` to a comma-separated list of `makerworld`,
`printables` and `thingiverse`. When the setting is unset or blank, the
`library` tool is not advertised and the server makes no request to these
sites. Parsing settings makes no network request.

| Site | Search and details | Download | Settings |
| --- | --- | --- | --- |
| MakerWorld | No account | Bambu Lab account | [MakerWorld sign-in](#makerworld-sign-in) |
| Printables | No account | No account for free models | None |
| Thingiverse | App token | App token | `PRINTABLE_THINGIVERSE_TOKEN` (required when enabled) |

Supply passwords, secrets and tokens through your deployment's secret provider.
Do not place them in committed configuration, commands, prompts or logs. The
server sends each token only to its own site's API host, never to file hosts.
`status` reports which sites can search and download and the MakerWorld sign-in
state, not the secret values.

A Thingiverse app token comes from the Thingiverse developer site. Reports
from 2026 disagree about whether new app tokens can still be created.

## MakerWorld sign-in

MakerWorld downloads need a Bambu Lab access token, which expires after about
90 days. Bambu Lab offers no way to renew one without signing in again, so the
server signs in for you:

| Setting | Purpose |
| --- | --- |
| `PRINTABLE_BAMBU_ACCOUNT` | The Bambu Lab account's email address. Accounts registered by telephone number are not supported |
| `PRINTABLE_BAMBU_PASSWORD` | Its password; required with the account |
| `PRINTABLE_BAMBU_AUTHENTICATOR_SECRET` | Optional. The base32 secret shown beside the QR code when you set up an authenticator app for the account. With it, the server answers authenticator codes itself |
| `PRINTABLE_LIBRARY_STATE_DIR` | Optional. An existing, writable absolute directory that only the server can read; the server refuses to start if it cannot write there. The session is kept there, owner-readable only, so a restart does not need a new code. Without it, the session is kept in memory |
| `PRINTABLE_MAKERWORLD_TOKEN` | Optional. A token to start with, such as the `token` cookie makerworld.com sets after you sign in in a browser. A newly set or changed token is used instead of a stored session. Without an account, this is the only way to download, and it must be replaced by hand when it expires |

The server signs in when a MakerWorld download needs a token: the first time,
after the token expires, or when Bambu Lab rejects it. It checks the request
first, so an invalid request never triggers a sign-in. A rejected token causes
one new sign-in and one retry. Bambu Lab often asks for a second step:

- **With an authenticator secret configured**, the server computes the
  authenticator app's current code and finishes the sign-in by itself. When a
  download happens during the last week before the session expires, it also
  renews the session; there is no renewal between downloads. If Bambu Lab
  refuses a computed code, the secret or the server clock is wrong: the server
  logs a warning, stops using the secret until it restarts, and asks the user
  instead. `status` then reports `authenticator_refused`.
- **Otherwise**, the download fails with `library_sign_in_required`. The message
  says whether Bambu Lab emailed a code or wants the authenticator app's code.
  The agent asks the user for it, then calls `sign_in` and retries the download:

  ```json
  {"action": "sign_in", "params": {"site": "makerworld", "code": "123456"}}
  ```

  While a code is awaited, further downloads and `sign_in` without a `code`
  report the same request for ten minutes rather than asking Bambu Lab to send
  more emails. After that, `sign_in` without a `code` starts a new sign-in.

Bambu Lab answers authenticator codes on its website host, `bambulab.com`,
which can refuse requests that do not come from a browser. When it does, the
server asks Bambu Lab to email a code instead and reports that. It does not
imitate a browser. If an early renewal would need a code from the user, it is
not retried for that session; the user is asked once the session expires. A
code Bambu Lab emailed during that renewal can still be given to `sign_in`.

A sign-in code passes through the conversation with the agent. It is valid for
one sign-in and expires within minutes; the password and authenticator secret
never do. Signing in can also fail with `library_challenged` when Bambu Lab
answers with a CAPTCHA, or `library_sign_in_rejected` for a wrong password or
code.

To keep the session across restarts with Compose, mount a named volume that
only the server uses at `/var/lib/printable-library` and set the state
directory. The server image owns that directory as UID 10001 with owner-only
access, and Docker gives a new named volume the same ownership:

```yaml
services:
  server:
    environment:
      PRINTABLE_LIBRARY_STATE_DIR: /var/lib/printable-library
    volumes:
      - library-state:/var/lib/printable-library
volumes:
  library-state: {}
```

A bind mount or network volume must already be writable by UID 10001.
Do not mount the volume into the Blender, CAD or slicer containers. If a later
write fails, the session stays in memory and `status` reports
`persistence_failed`.

## Use

`search` lists models by keyword, one site at a time:

```json
{"action": "search", "params": {"site": "printables", "query": "bowl", "sort": "popular", "limit": 10}}
```

`sort` is `relevance` (default), `popular` or `newest`. MakerWorld does not offer
`newest`. `limit` is 1 to 50; Thingiverse returns at most 30 per page and reports
the limit it used. Results include each model's `model_id`, page URL, designer, license
when the site reports one, image URL and counts. Follow `next_page` for more.

`get` describes one model by its `model_id` or by a pasted page link, and lists
its files:

```json
{"action": "get", "params": {"site": "makerworld", "model": "https://makerworld.com/en/models/1514079-mesh-bowl"}}
```

Each file has a `file_id`, name, kind (`model`, `print_profile`, `sliced` or
`other`), and its size when the site reports it. MakerWorld files are print
profiles: 3MF projects with the designer's printer settings.
`downloads_available` reports whether this server can download from the site.

`download` saves selected files into a project:

```json
{"action": "download", "params": {"site": "printables", "model": "553277", "files": ["stl-2351722"], "project_id": "kitchen"}}
```

Files go to `projects/<project_id>/library/<site>-<model_id>/` unless
`directory` names another project-relative directory. File names are reduced to
letters, digits, `-`, `_` and `.` with a lowercase extension. Only workspace
artifact types are accepted; a PDF or other unsupported file is refused before
any transfer.

A `source.json` record in the same directory keeps the site, model ID, page URL,
title, designer, license, retrieval time, and each saved file's site name,
path, size and SHA-256. The result returns the same details. A site file that is
itself named `source.json` is saved under another name.

Every requested file is received into managed scratch space before any of them
is committed, so a failed transfer leaves the project unchanged. An existing
file at a destination, including `source.json`, is refused before any transfer
unless `overwrite` is true. If committing fails after some files were saved, the error code is
`library_incomplete` and the message names the saved files.

After download:

- Import STL with `scene` `import`, and check it with `validate_mesh`.
- Prepare STL or 3MF for printing with `slice` `prepare`.
- Convert STEP with `cad_build` `import_step`.
- Use `blender_execute` for other formats.

## Network behavior

- API requests go only to `api.bambulab.com`, `api.printables.com` and
  `api.thingiverse.com`.
- File downloads follow at most five redirects. Every hop must use HTTPS on the
  default port and stay on the site's file hosts:
  - MakerWorld: `makerworld.bblmw.com`, `public-cdn.bblmw.com` and path-style
    Amazon S3 hosts such as `s3.us-west-2.amazonaws.com`.
  - Printables: `files.printables.com`.
  - Thingiverse: its API host, `cdn.thingiverse.com` and `www.thingiverse.com`.
- Signed download links are used exactly as the site issued them.
- Only Thingiverse receives its token on a download, on the first request to
  its API host alone. No token follows a redirect.
- Each file transfer must finish within one hour and must not stall for more
  than 60 seconds.
- Ambient proxy settings are ignored.
- Requests identify the client as `printable/<version>`.
- API responses are limited to 8 MiB.
- Each file is limited by `PRINTABLE_FILE_UPLOAD_MAX_MIB` and the workspace
  storage budget.
- At most two library requests run at once.

MakerWorld sign-in also uses `bambulab.com` for authenticator codes.

The server container needs outbound HTTPS to these hosts. The Blender, CAD and
slicer containers do not, and they never receive the tokens, passwords or
secrets.

## Errors

| Code | Meaning |
| --- | --- |
| `library_credentials_required` | The site needs a token or account that is not configured |
| `library_authentication` | The site rejected the token; it may have expired |
| `library_sign_in_required` | MakerWorld needs a code from the user; the message says which code and how to submit it |
| `library_sign_in_rejected` | Bambu Lab refused the password or code; for an expired emailed code, Bambu Lab has sent a new one |
| `library_forbidden` | The site refused the model or file, for example because it is paid, Club-only, needs points or needs an account this server does not use; the site's reason is included when it gives one |
| `library_not_found` | No such model or file |
| `library_challenged` | The site asked for a CAPTCHA; wait before trying again, or download the file in a browser and upload it with `artifact` |
| `library_rate_limited` | The site is limiting requests; wait before trying again |
| `library_unavailable` | The site could not be reached, failed, or the transfer was interrupted |
| `library_incompatible` | The site rejected this version's request, with the site's reason when it gives one, or returned a response this version does not understand or that exceeds the size limit |
| `library_file_too_large` | The file exceeds `PRINTABLE_FILE_UPLOAD_MAX_MIB` |
| `library_disallowed_host` | A download redirected outside the site's file hosts |
| `library_incomplete` | Committing failed after some files were saved |

## Licenses, site terms and limits

Titles, summaries and file names come from the site and are untrusted text. Each
design keeps its own license, which is recorded in `source.json`. Check it before
remixing, selling or redistributing a design.

Site terms also apply. MakerWorld's terms of use restrict automated access to
its content, so operators decide whether to enable it. Printable does not solve
CAPTCHAs, imitate a browser, or bypass paid or Club-only access.

MakerWorld and Printables do not document the APIs these clients use, and the
sites can change them without notice. The unit tests run against local fake
sites; they do not show that the live sites still behave the same way.
Printables search results do not say whether a model is paid. Downloading a paid
model fails with `library_forbidden`. MakerWorld files that are not part of a
print profile are not offered.

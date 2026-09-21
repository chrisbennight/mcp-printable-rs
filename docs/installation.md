# Install on a Linux NVIDIA host

Printable runs an authenticated HTTP MCP server, a live Blender process with a
private display, and a separate Blender render worker. This installation is for
one trusted person or team. Everyone holding the bearer can modify the shared
scene, read workspace files, and execute Python inside Blender. It is not a
multi-tenant service or a Python sandbox.

Use Linux/amd64, Python 3.11 or newer for the supplied client and test scripts,
Docker Engine with the Compose plugin, an NVIDIA GPU supported
by Blender, and the [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html).
Check `nvidia-smi` on the host before starting and qualify the intended GPU
against the exact image set. NVIDIA remains required for this
installation. Software graphics tests in CI do not establish GPU compatibility.

The supplied container limits allow 10 GiB of RAM across the three application
services. Leave additional memory for the host and image builds. This is a
configured budget, not a measured minimum. Workspace storage grows with saved
models and render frames; Docker named volumes have no capacity limit here.
Provision a dedicated filesystem or host quota and monitor its free space.

## Build and configure

Start from a clean checkout of an approved commit. Until a qualified image set
is published, build the server, Blender, and CAD images from that same checkout:

```sh
git clone https://github.com/chrisbennight/mcp-printable-rs.git
cd mcp-printable-rs
git diff --exit-code
git diff --cached --exit-code
revision=$(git rev-parse HEAD)
docker build --build-arg SOURCE_REVISION="$revision" -t printable-server:local .
docker build --build-arg SOURCE_REVISION="$revision" -f blender/Dockerfile -t printable-blender:local .
docker build --build-arg SOURCE_REVISION="$revision" --target cad-runtime -t printable-cad:local .
python3 scripts/create-local-secret
docker image inspect --format '{{.Id}}' printable-server:local
docker image inspect --format '{{.Id}}' printable-blender:local
docker image inspect --format '{{.Id}}' printable-cad:local
```

Create `.dev/compose.env` with the three image IDs printed above:

```dotenv
PRINTABLE_SERVER_IMAGE=sha256:REPLACE_WITH_SERVER_IMAGE_ID
PRINTABLE_BLENDER_IMAGE=sha256:REPLACE_WITH_BLENDER_IMAGE_ID
PRINTABLE_CAD_IMAGE=sha256:REPLACE_WITH_CAD_IMAGE_ID
PRINTABLE_GPU_DEVICE=0
PRINTABLE_PORT=8000
```

The image IDs prevent a later local tag change from silently replacing a component.
A published image set instead uses its verified registry references with
`@sha256:` digests. Do not combine images from different revisions.

The secret helper creates `.dev` with mode `0700` and a cryptographically random
bearer. It preserves an existing credential. Compose bind-mounts the secret
file only into the server; its file mode permits container UID 10001 to read it,
while the private parent directory restricts other host users. Keep `.dev`
private. Do not paste the bearer into commands, issue reports, or client logs.

`PRINTABLE_GPU_DEVICE` selects a device index or UUID exposed by the NVIDIA
runtime. Both Blender processes share that allocation; they do not reserve the
whole physical GPU or stop other GPU users. See Docker's
[device reservation guidance](https://docs.docker.com/compose/how-tos/gpu-support/).

```sh
docker compose --env-file .dev/compose.env config --quiet
docker compose --env-file .dev/compose.env up -d --wait --wait-timeout 600
python3 scripts/printable_client.py status
```

The server listens at `http://127.0.0.1:8000/mcp`. Its `/healthz` endpoint checks
process liveness and `/readyz` reports dependency readiness. The status command
also reports live Blender and render-worker state. Correct missing or unhealthy
dependencies before modeling; a healthy HTTP process alone is not a ready
modeling service.

The one-shot initialization service changes ownership of the dedicated
workspace volume root. Application services run as UID 10001 with read-only
root filesystems, dropped capabilities, bounded temporary storage, and no host
display or Docker socket. Blender has no published ports and uses an internal
control network. The server additionally has a frontend network for its
loopback port; this configuration does not prohibit server outbound traffic.

## Make and retrieve a bracket

Run the deterministic tutorial on the new installation:

```sh
python3 scripts/quickstart.py .dev/first-bracket
```

It saves the previous live scene, replaces it with a bracket, compiles and
validates its STL, produces a dimensioned PNG, saves a Blender checkpoint, and
submits a small turntable job to the background worker. It downloads the STL,
PNG, BLEND, and MP4 into the new directory and records inspection, validation,
and job metadata there. The previous checkpoint path and submitted job ID are
printed. Choose a new output directory for another run; do not retry a failed
mutation blindly. Inspect the saved metadata and service status first.

The bracket is a demonstration, not a certified load-bearing part. Review the
validation report and dimensions before slicing and printing.

## Connect a client

Configure an HTTP MCP client with the endpoint above and an Authorization
header using the bearer from `.dev/mcp-bearer` through that client's protected
credential settings. The protocol is streamable HTTP; stdio is not supported.
The included direct client reads the credential file itself:

```sh
python3 scripts/printable_client.py call inspect examples/quickstart/inspect.json
python3 scripts/printable_client.py download model.stl ./model.stl
```

The download command publishes an immutable snapshot, requests a one-use HTTP
grant, streams the bytes, and verifies size and SHA-256 before creating the
destination. It refuses redirects and overwriting existing files. Its download
limit is 1 GiB. Ordinary MCP clients may support tools but lack the custom
`files/authorizeDownload` extension; use this command to retrieve their files.
No gateway is required, and file bytes are not returned as chat text.

## Reverse proxy and browser requests

For remote access, terminate HTTPS at a trusted proxy and keep the upstream
port on loopback or an equivalently restricted network. Set the external Host
forms in `PRINTABLE_ALLOWED_HOSTS`. Set `PRINTABLE_DOWNLOAD_BASE_URL` explicitly,
for example `https://printable.example.com/parts/`. A proxy using that prefix
must route `/parts/mcp` to `/mcp` and `/parts/file-transfers/` to
`/file-transfers/`, preserving authorization and session headers. Disable
response buffering for MCP event streams. Neither forwarded Host nor forwarded
scheme headers control upload or download links. Both transfer directions use
the configured base URL; the setting retains its existing download-oriented name.

Browser Origin headers are rejected by default. If a trusted browser client is
needed, set `PRINTABLE_ALLOWED_ORIGINS` to its exact scheme, host, and optional
port. This setting does not enable CORS or replace bearer authentication.
Non-browser clients normally send no Origin header.

## Stop, restart, and credentials

```sh
docker compose --env-file .dev/compose.env stop
docker compose --env-file .dev/compose.env up -d --wait --wait-timeout 600
```

Models and job metadata persist in the named workspace volume. A live scene is
not a saved checkpoint: save it with `scene` action `checkpoint` before stopping.
Restore that checkpoint explicitly after restarting. Isolated render jobs use
their persisted source snapshots and resume recorded progress; see
[render recovery and rollback boundaries](render-worker.md#runtime-identity-and-recovery).

For bearer rotation, stop the server, replace the credential through a protected
local process with the same format and permissions, recreate the server so its
secret mount uses the new file, and update client credential stores. The server
reads credentials at startup. Never remove the workspace volume to rotate a
credential. Avoid `docker compose down --volumes`: it deletes saved work.

See the [automated recovery evidence](../evidence/installation-recovery.md) for
the tested backup, damaged-metadata, and disk-exhaustion boundaries.

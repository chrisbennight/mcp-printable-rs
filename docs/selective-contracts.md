# Load one action contract with the direct client

The direct client can read Printable's compact contract index and then one
tool or action contract. This is useful when an agent can run the client and
choose which results enter its context. Standard MCP hosts may still expose
every `tools/list` schema automatically; these commands do not change a host's
behavior or its authorization rules.

With an isolated installation and the bearer file from the
[installation guide](installation.md), read the available names and select
the operation you need:

```sh
python3 scripts/printable_client.py contracts
python3 scripts/printable_client.py contracts scad_build mesh
python3 scripts/printable_client.py contracts validate_mesh
```

The first command returns the index. The next commands return only the
requested contracts, including their input schemas, output schemas,
descriptions, and whole-tool annotations. They use `resources/read`; they do
not call `tools/list`. Selected action output schemas identify whether their
scope is the action or the whole tool. Typed printer contracts are preserved.

Invoke the normal public tools after reading their contracts:

```sh
python3 scripts/printable_client.py call scad_build examples/quickstart/discovery-build.json
python3 scripts/printable_client.py call validate_mesh examples/quickstart/discovery-validate.json
```

The fixture produces a box with dimensions 10 by 20 by 30 millimetres at
`examples/discovery/part.stl`. Run against an isolated workspace, or choose an
unused destination in both JSON requests. The build refuses an existing
destination. Inspect an uncertain outcome before retrying a mutation.

Full discovery is also available and follows pagination:

```sh
python3 scripts/printable_client.py tools
```

Tool calls prefer `structuredContent` when it is present. If it is absent,
the client accepts a single JSON text result. A duplicate text representation
is not printed alongside the structured result. MCP `isError` remains an
error, and mutations are never automatically replayed.

## Record evidence on an isolated installation

```sh
python3 scripts/selective_client_smoke.py --output .dev/discovery-evidence
```

The output directory must not exist. The example runs the same box build and
dimension check after full and selected discovery, using separate artifact
paths. It retains the actual exposed contracts, ordered RPC requests/results,
call durations, decoded JSON sizes, task outcomes, client source digest,
client/server versions, and protocol version. It does not invoke a model.
Model identity and provider usage are explicitly unavailable, and JSON bytes
are never reported as tokens. No images are requested in this fixture.

The container installation test runs this example against its temporary
workspace and additionally records image IDs and source revisions. Pass
`--evidence-dir` to `scripts/test-installation.py` to retain its `discovery`
directory alongside the verified tutorial artifacts. A standalone invocation
cannot identify server image digests through MCP and records runtime identity
as unavailable.

CI retains these records as the `discovery-evidence` artifact. Failed trials
retain completed responses and identify calls whose response was unavailable;
exceptions still fail the run, and uncertain mutations are not replayed.

These are single, ordered observations with uncontrolled cache effects, so
their elapsed times are not a controlled speed comparison. The retained
contracts show what this client exposes, not what an arbitrary host injects
into a model. Measure actual provider usage in the consuming host before
claiming token savings. Existing server-wide discovery and authorization
remain available for hosts that require them.

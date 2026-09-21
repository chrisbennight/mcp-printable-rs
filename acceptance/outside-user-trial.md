# Independent installation trial

Status: not yet performed by a person unfamiliar with this repository.
Automated CI and an agent running the tutorial are useful regression evidence,
but neither substitutes for this trial.

Ask a person who has not worked on Printable to use the README and installation
guide on a clean supported Linux/amd64 NVIDIA host. Let them identify confusing
steps before providing maintainer help. Record the exact source revision or
image pair, GPU/driver, host resources, Docker/Compose versions, client, and
time spent. Exclude credentials and unrelated personal information.

The participant should:

1. Explain what the service does and identify its supported platform and trust boundary.
2. Build or obtain a matching pair, create the credential, and start the services.
3. Distinguish a healthy HTTP process from ready modeling and render dependencies.
4. Run the bracket tutorial and locate the STL, dimensioned PNG, checkpoint, and video.
5. Make a deliberate parameter change through a client and check the resulting dimensions.
6. Retrieve an artifact without a gateway and understand a failed or expired download.
7. Save and restore a checkpoint, inspect a render job, and restart the server.
8. Find backup, storage, rollback, support, and private security-reporting guidance.

For every step record completion, help required, misleading wording, unexpected
errors, and the artifact or observation proving the result. Record failures as
well as successes. Do not change the acceptance threshold after seeing results.
Fix documentation defects and repeat affected steps with the revised guide.

Use two independently implemented HTTP MCP clients for compatibility evidence;
record their names and versions and which capabilities each actually exercised.
The included Python direct client and Rust external smoke provide automated
protocol coverage, but do not establish support for every third-party client
or for clients that lack the custom file-download extension.

No model-token or latency improvement is claimed without actual harness and
resource measurements. Follow the broader [agent evaluation plan](agent-workflows.md)
for controlled modeling trials; this installation trial is a separate outcome.

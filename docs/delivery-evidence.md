# Design and print evidence

Printable links retained records using a canonical workspace path and a SHA-256
digest. Each record has `format_version: 1`, a kind, a project identifier, and
operation-specific data. These references identify immutable local evidence;
they do not certify a physical part or independently attest to a printer backend.

1. Build a project revision and retain its CAD measurement reference when one
   is available. Pass that reference as `design_record` to `slice.prepare`.
   The slicer verifies the record digest and the selected source artifact hash
   and path before starting native slicing. Available revision, requirements, delivery
   qualification, and CAD engine version travel with the slice. A source
   without retained CAD evidence remains supported with an unverified design
   link.
2. A completed slice returns `provenance`. Its retained record contains the
   source hash, effective resolved settings and their hash, the request,
   slicer identity, and output artifact hashes. Changing a source or settings
   produces a different record even if a printer artifact happens to be identical.
3. `slice.review` returns another `provenance` reference. It links the exact
   slice, selected G-code digest, preview parameters, and a retained preview
   image. This records which toolpath was rendered; it does not record human
   approval or establish that unreviewed layers were inspected.
4. Supply these two references to `print.import` as
   `evidence: {"slice": <slice reference>, "toolpath_review": <review reference>}`.
   A review requires its slice. Import verifies the local upload snapshot
   against the slice's printer artifact and the review's exact slice identity.
   Changed bytes or an earlier review from different settings are rejected
   before upload. Import without these optional references remains available
   and reports unverified links.
5. Import retains the exact uploaded bytes, local digest, intent, and successful
   receipt. Its `provenance` reference points to the receipt, which links the
   intent and earlier evidence. `backend_digest_verification: "unverified"`
   means Printable cannot confirm the bytes stored or executed by Bambuddy.
6. For library files imported with a retained receipt, `print.stage` and
   `print.start` save an intent before sending the mutation and retain an
   accepted submission afterward. Their `delivery_evidence.execution` remains
   `not_observed`, even after an accepted start. `print.status` reads the actual
   queue record and retains a separate observation: `printing_observed`,
   `completion_observed`, or `not_observed`. These are backend observations,
   not physical inspection or digest verification.

Use `artifact.read` for retained metadata or `artifact.publish` to transfer larger
files. Responses carry compact references; full settings and records stay in the
workspace. Queue observations link up to 100 known submission receipts; the
retained record explicitly reports whether that list is complete. Repeated
identical observations reuse the same record. The response includes the fetch
time; backend start/completion times remain in the observed queue data.

Submission requests, acknowledgements, and later queue observations are separate
snapshots. They do not prove that a backend executed unchanged settings. Compare
the retained setup when diagnosing changes, and retain the unverified backend
digest status. Printer review of hardware/material compatibility remains a
separate operation from rendering a toolpath preview.

A failed or disconnected mutation is never replayed automatically. Its retained
intent stays unknown when no receipt was saved. Inspect the backend and retained
intent before another mutation. If receipt persistence fails after acceptance,
the error identifies the known library or queue identifier. Submission intents
are under `.printable/evidence/submission`; accepted queue links are under
`.printable/evidence/print_jobs/<print id>`. Import intents and receipts have
their own directories under `.printable/evidence`.

Historical slices and imports remain readable. Missing retained records do not
acquire invented revision, review, or submission history. Manual library files
and archives continue to work with no local receipt link; a null record or absent
historical reference is unverified. All retained evidence and upload copies must
be preserved with the workspace; they are not disposable temporary files.

Tests use fake CAD/slicer outputs and isolated fake printer services. They cover
changed upload paths, settings and source changes, retained images and bytes,
cross-project references, accepted requests versus observed execution, and
repeated status observations. They do not start physical printers.

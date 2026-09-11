# Agent-driven Printable product refactor

This document records the earlier product refactor and private deployment
tracking. Its capability contracts remain useful design context; its lab issue
links and deployment authorization are historical. Use the current
[architecture](architecture.md), [workflow interface](workflow-interface.md),
and GitHub issues for independent installation and new work.

## Product outcome and delivery status

Deliver Printable as one coherent agent-driven modeling, inspection, validation, and rendering product. Agents must be able to create and revise models, investigate the actual Blender editing state, verify manufacturing and motion evidence, and deliver recoverable artifacts without repeatedly loading large tool catalogs or verbose scene dumps.

This is a product refactor, not a collection of additional wrappers. Success means the complete agreed interface and native observation workflow are live, existing useful capabilities remain available, and representative agents complete tasks reliably within measured context and runtime budgets. Commit, PR, review, deployment, and live verification are authorized under pr-and-monitor.

## Complete target interface

The advertised server exposes exactly these unprefixed tool names; the gateway supplies the Printable namespace. Actions/variants live in typed arguments, not separate lifecycle tool names.

| Tool | Product responsibility |
|---|---|
| status | Readiness, active work, capabilities, and scene identity |
| inspect | Bounded scene, object, material/modifier/hierarchy, node-tree and editing-state queries |
| edit | Existing typed primitive, boolean, rename, and rigid-rotation authoring operations |
| blender_execute | General Blender Python with bounded output, explicit execution context, and recovery semantics |
| scene | Clear, checkpoint/save, restore, model import and export |
| scad_build | OpenSCAD validated STL, PNG, and SVG section output variants |
| view | Native viewport/editor observations and controlled dimension, section, and overhang diagnostics |
| render | Authored or product-presentation images, single views, galleries, and turntable contact sheets |
| compare_renders | Compare retained image artifacts |
| validate_mesh | Solid defects, dimensions, mass properties, bed contact, and overhang evidence |
| analyze_assembly | Interference, clearance, and continuous rigid-motion evidence |
| job | Submit/list/inspect/cancel durable renders; progress, certificates, and output references |
| artifact | Workspace file listing/read/write/publication and multipart transfer |

Jobs are render execution. Upload handles and chunking belong exclusively to artifact transfer. Mixed-action tools receive conservative whole-tool MCP annotations; an action argument is not an independent authorization boundary.

## Architecture and intent

- One authoritative live Blender scene runs normal Blender with a private GPU-accelerated virtual display. Python and typed operations remain the primary action interface; native viewport/editor capture supplies observation.
- A separate bounded background rendering worker consumes immutable checkpoints. It has no second editable source of truth and never synchronizes model changes back to live Blender.
- Preserve resource-aware admission on the shared NVIDIA GPU. Separate processes do not imply unlimited concurrency or guaranteed responsiveness under GPU exhaustion.
- No public desktop, Docker socket, privileged runtime, host PID, unrelated mounts, or unrelated workload control. Display configuration must prove useful native GPU operation within the approved isolation boundary.
- Native viewport drawing, exact editor-region capture, and controlled diagnostic rendering are distinct capture methods. Never silently substitute one while claiming identical overlays or editing-state fidelity.
- Reuse first-party geometry, OpenSCAD, file confinement, image verification, certification, and recovery implementations. Consolidation must not weaken these contracts.

## Agent feedback contract

Agents can inspect precise data, execute edits, actively change viewpoints, examine native selection/topology/shading/node/UV state, and compare retained observations. Captures identify scene generation, model revision, frame, view configuration, method, and convergence where relevant. Dependency evaluation/redraw completion must precede a fresh capture. Recreated UI/GPU handles after file restore or restart cannot be mistaken for previous state.

Scene edits and inspection-view changes have distinct state semantics. Stale model expectations are checked at the authoritative serialized mutation boundary. Timeouts with unknown mutation outcomes require reconciliation; arbitrary Python is not promised transactional rollback.

Provide compact structured outputs and output schemas, bounded pagination and projection, concise discovery descriptions, reusable view definitions, optional targeted post-edit observation, and artifact-backed history. Large files and repetitive job polling stay outside model context through the gateway and Code Mode. Image bytes are not a model-independent token metric.

## Capability preservation and consolidation

- scene_get/object_get/node_tree_get -> inspect.
- primitive_create/boolean_apply/object_rename/rigid_rotation_animate -> edit.
- scene_clear/checkpoint/restore, blend_save, stl_import/export -> scene.
- scad_compile/render/cross_section -> scad_build.
- render_dimensions/cross_section/printability_heatmap -> view, plus native observation.
- render_preview/product/gallery/turntable -> render.
- render_job_submit/status/list/artifacts/cancel -> job.
- workspace_list/read/write/publish/write_begin/write_chunk/write_commit -> artifact.
- status, blender_execute, compare_renders, validate_mesh, analyze_assembly retain their distinct responsibilities with unprefixed names.

Verify current consumers and gateway discovery before removing advertised legacy aliases. Temporary adapters may share implementation during migration; the final production catalog is the agreed thirteen tools, not both catalogs indefinitely.

## Evidence and limits

Research supports code-execute-observe-revise and active multiview investigation, not a claim about undisclosed gpt-6-astra training or universal superiority of desktop screenshots.

- [BlenderGym](https://arxiv.org/html/2504.01786): Python-driven graphics editing, generator/verifier loops, and multiview treatment of occlusion.
- [VIGA / BlenderBench](https://arxiv.org/html/2601.11109v2): iterative code/render/inspection, viewpoint control, bounded evolving history, and evaluation without model fine-tuning.
- [Blender GPU types](https://docs.blender.org/api/current/gpu.types.html): native viewport drawing with SpaceView3D/Region contexts.
- [Blender GPU module](https://docs.blender.org/api/current/gpu.html): GPU context lifetime.
- [Blender operators](https://docs.blender.org/api/current/bpy.ops.html) and [application timers](https://docs.blender.org/api/current/bpy.app.timers.html): UI-context-aware execution and main-thread scheduling.
- [Community Blender MCP capture](https://github.com/ahujasid/blender-mcp/blob/main/addon.py): offscreen drawing with window-capture fallback; inspect capture fidelity rather than assuming all overlays match.
- [NVIDIA container capabilities](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/docker-specialized.html) and [headless NVIDIA display guidance](https://virtualgl.org/Documentation/HeadlessNV): technical grounding, not proof of our exact deployment.
- [Effective agent tools](https://www.anthropic.com/engineering/writing-tools-for-agents), [programmatic tool calling](https://platform.claude.com/docs/en/agents-and-tools/tool-use/programmatic-tool-calling), and [MCP tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools): workflow boundaries, structured results, selective context, and explicit state. These were also consulted through Grounded Docs' mcp-api-design library.

## Delivery order

1. Establish complete outcome-based contracts, capability mapping, and representative evaluation baseline.
2. Consolidate catalog/shared result contracts while preserving behaviors and preparing coordinated migration.
3. Deliver GUI-backed runtime and verified native capture, with state/freshness integration.
4. Isolate durable rendering from editable live state using immutable checkpoint workers.
5. Integrate workflows, evaluate actual agents and context use, then cut over gateway/deployment and remove legacy public aliases.

Child issues track reviewable outcomes and dependencies. Every PR must cite its parent outcome, preserve the aggregate scope, pass required CI and AERB disposition, and be monitored through merge. Do independent authorized work before escalating a human-only deadlock.

## Epic completion criteria

- [ ] Complete agreed public interface is live and discoverable; all prior capability mappings have public regression evidence.
- [ ] Native GPU-backed viewport and relevant editor feedback are usable and truthful about capture method, state, and unsupported features.
- [ ] Agents can actively investigate views and receive exact bounded state without routine scene/history dumps.
- [ ] Long checkpoint-based rendering does not replace or mutate the authoritative live scene; contention and recovery are explicit.
- [ ] Manufacturing and mechanical evidence remains bound to exact analyzed inputs; no image or sampled motion substitutes for certification.
- [ ] Existing product corpus and held-out agent tasks pass with recorded correctness, failures, calls, text/image usage, latency, and resource results.
- [ ] Production release pair/worker configuration, gateway catalog, file handoff, restart recovery, and live user workflow are verified.
- [ ] Child issues are closed on observed delivery, not merely code merge.

External asset providers, general mesh repair, arbitrary GUI mouse automation, and saved-script registries are outside this refactor unless separately authorized. Preserve extension paths without growing the tool catalog speculatively.

## Linked delivery issues

| Issue | Outcome | Depends on |
|---|---|---|
| [#94](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/94) | Define agent workflow acceptance and baseline for the Printable product refactor | — |
| [#95](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/95) | Expose the complete thirteen-tool Printable interface with structured contracts | [#94](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/94) |
| [#96](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/96) | Bind agent edits and observations to explicit Blender state and execution context | [#94](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/94), [#95](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/95) |
| [#97](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/97) | Run authoritative Blender with a private GPU-backed UI and reliable event loop | [#94](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/94) |
| [#98](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/98) | Give agents native viewport/editor feedback and flexible diagnostic investigation | [#95](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/95), [#96](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/96), [#97](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/97) |
| [#99](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/99) | Render immutable checkpoints without occupying or replacing the live Blender scene | [#95](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/95), [#96](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/96), [#97](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/97) |
| [#100](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/100) | Prove coherent agent modeling workflows and context efficiency across the refactored product | [#94](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/94), [#95](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/95), [#96](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/96), [#97](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/97), [#98](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/98), [#99](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/99) |
| [#101](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/101) | Cut over Printable to the coherent interface and verify the complete product live | [#95](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/95), [#96](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/96), [#97](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/97), [#98](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/98), [#99](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/99), [#100](https://gitea.cacahuate.org/bennight/mcp-printable-rs/issues/100) |

The links above are the delivery graph. Earlier slices may merge independently, but the epic stays open until the complete product is live and verified.

## Current implementation boundary

The source now implements the thirteen-tool catalog, native UI capture, isolated render workers, explicit editor context, scene revision preconditions, and compact structured results. The release smoke checks actual public results against discovered output schemas and exercises native feedback with independent worker rendering. Source implementation is distinct from verified deployment: the completion checklist remains open until coordinated production/gateway cutover and the [agent evaluation plan](../acceptance/agent-workflows.md) have retained delivery evidence.

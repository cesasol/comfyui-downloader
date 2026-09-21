# Model placement heuristics

Status: implemented on 2026-09-17. See [Implementation notes](#implementation-notes)
for where each part lives, the evidence gathered for the alias defaults, and the
three places where the shipped behavior departs from the text below.

## Purpose

Organize model files for navigation while creating ComfyUI workflows. A flat
list of many models is difficult to use. Family folders are browsing categories,
not guarantees that diffusion weights, encoders, VAEs, or LoRAs are compatible.

New placements use one of these shapes:

```text
models_dir / <role> / <family> / <file>
models_dir / <role> / <file>             # family unresolved
```

Classify each file by its own model family, not by every workflow that uses it.
Trust explicit role declarations. Preserve meaningful unknown family labels.
Create independent placements through downloads or reflinks, never symlinks.

## Agreed behavior

| Concern | Decision |
|---|---|
| Folder meaning | Navigation by model family, not compatibility or workflow collections |
| Family namespace | Project-owned labels with explicit source aliases |
| Unknown family | Preserve a meaningful label or umbrella; otherwise use flat placement |
| Classification | Evidence-first, per file |
| Overrides | Per-file family overrides and persistent configurable aliases |
| Config defaults | Populate a user-owned snapshot; update through an explicit merge command |
| Role authority | Explicit user and template declarations override inference |
| Shared content | Reflinks only when creating a placement from existing verified content |
| Clone failure | Fail the new placement; no link, full-copy, or alternate-path fallback |
| Late duplicates | Keep independently downloaded files |
| Identical requests | Coalesce into one placement |
| Destination conflict | Verify content, then fail if different or unverifiable |
| Lifetime | Deleting one distinct placement must preserve all others |
| Existing files | Preserve established paths, including during routine updates |

## Current implementation and evidence

These implementation observations were checked against the source during review.
They describe the starting point, not the proposed behavior.

| Area | Current behavior | Source |
|---|---|---|
| Template metadata | Index families survive in `TemplateEntry.model_families` and `TemplateBundle` | `src/templates/mod.rs`, `parse_index` |
| Model parsing | Parses properties and MarkdownNote links; duplicate resolved URLs favor properties | `src/templates/mod.rs`, `parse_workflow_models` |
| Template queueing | Sends URL and role, and deduplicates URLs across selected bundles | `src/cli/mod.rs`, template picker |
| Persistence | Queue items and catalog jobs lack a durable family field | `src/ipc/protocol.rs`, `src/catalog/mod.rs`, `src/catalog/schema.rs` |
| Source detection | Accepts direct HuggingFace URLs as well as template URLs; HF does not imply template provenance | `src/daemon/downloader.rs`, `resolve_version` |
| Header routing | Checkpoint rerouting applies to HF jobs too; HF resolution sets no base model | `src/daemon/downloader.rs`, `download`, `resolve_huggingface` |
| Header detection | Recognizes `first_stage_model.*`, `cond_stage_model.*`, and `conditioner.embedders.*` | `src/safetensor.rs`, `inspect_components` |
| Dedup | Hash hits return before destination computation, header inspection, and sidecar writing | `src/daemon/downloader.rs`, `download` |
| Existing destinations | Existing-path reuse does not verify the file hash | `src/daemon/downloader.rs`, `download` |
| Filename resolution | Pre-transfer checks use metadata names; transfers can use Content-Disposition or URL names | `src/daemon/downloader.rs`, `download` |
| Catalog identity | Jobs hold a destination and hash; there is no canonical-file ownership model | `src/catalog/mod.rs` |
| Concurrent transfers | Jobs become reusable hash candidates after completion | `src/daemon/queue.rs` |
| Scanner | Skips symlink entries through file-type checks; scans a fixed subset of roles | `src/daemon/scanner.rs` |
| Deletion | Removes the catalog row before unlinking files; lacks shared-path reference protection | `src/catalog/mod.rs`, `src/daemon/mod.rs` |
| Updater | Can relocate existing files using the shared routing helper and raw base-model folders | `src/daemon/updater.rs` |

### Evidence limits

The earlier draft reported a 2026-09-16 snapshot of 287 cached workflows,
246 referencing HF weights, and 85 with multiple family labels. It also reported
role frequencies and cross-family file counts. Those aggregate measurements were
not independently reproduced during this review. They are not acceptance criteria.

Any replacement measurement must use both model sources parsed by
`parse_workflow_models`. A query over `nodes[].properties.models` alone misses
MarkdownNote links. A filename match alone does not establish content identity,
role authority, or per-file family attribution.

The earlier draft reported an LTXV file placed under `diffusion_models` despite
a template declaration of `checkpoints`. The routing mechanism permits this;
the particular on-disk observation was not independently reproduced.

Cached evidence distinguishes Stable Audio Open from ACE-Step. The inspected
CivitAI enum associates ACE with `AceStepAudioInput`. Do not ship the proposed
`Stable Audio` to `ACE Audio` alias without evidence establishing equivalence.

The upstream [CivitAI base-model records](https://github.com/civitai/civitai/blob/main/packages/civitai-shared/src/basemodel.constants.ts)
are a source of names, not a complete family registry or proof of compatibility.
The previous full enum, activity counts, and broad compatibility claims are not
retained as verified facts. Record an upstream revision when adding evidence.

## Role resolution

Resolve role independently from family. Persist how the role was obtained.

1. An explicit per-file user role takes precedence.
2. Otherwise, a template-declared directory is authoritative for that file.
3. Without a declaration, infer from source metadata, such as CivitAI `ModelType`,
   or normalized repository path information.
4. Use `other` if no role can be resolved.

Explicit declarations must pass path and role validation. Authority does not
permit absolute paths, traversal, or writes outside `models_dir`.

Do not reroute an explicitly declared role based on safetensors headers or GGUF
format. This applies to user declarations as well as template declarations.
Inspection for verification or diagnostics may still occur.

A role inferred from an HF repository path remains inferred. Do not use an HF
hostname, an `is_template` shortcut, or `model_version.is_some()` as a substitute
for declaration provenance.

### Inferred checkpoint corrections

Retain header-based inference only for undeclared checkpoint roles. Use raw
source family metadata for architecture-sensitive routing, not editable browsing
aliases. Renaming a browsing category must not change the inferred loader role.

Fold family names to lowercase alphanumeric characters before testing the
keep-in-checkpoints exception. Rename or document `is_video_checkpoint` to
express its actual purpose: a checkpoint-placement exception, not media type.

| Folded prefix | Proposed inferred checkpoint behavior | Evidence status |
|---|---|---|
| `ltxv` | Keep in `checkpoints` | Earlier template observations support this; retain representative fixtures |
| `hunyuan3d` | Keep in `checkpoints` | Earlier template observations support this; retain representative fixtures |
| `wan` | Remove the existing keep-in-checkpoints exception | Earlier WAN template observations place diffusion weights in `diffusion_models` |
| `hunyuanvideo` | Remove the exception | Earlier Hunyuan Video template observations support `diffusion_models` |
| Bare `hunyuan` | Remove the catch-all | Current code also matches `Hunyuan 1`, not just video or 3D families |
| Existing CogVideo and Mochi matches | Retain provisionally | Placement remains unverified; do not silently broaden these rules |

Removing an exception does not force every file in that family into
`diffusion_models`. A detected bundled VAE or CLIP still keeps an inferred
checkpoint in `checkpoints`. Preserve conservative behavior on inspection failure.

The current unspaced `hunyuanvideo` test misses `Hunyuan Video`; the bare
`hunyuan` test catches it and also catches `Hunyuan 1`. A regression test must
assert the corrected behavior, not claim that `Hunyuan 1` never matched.

The inspector does not recognize `vae.*` as a bundled VAE today. Extending
recognition to supported layouts needs fixtures and is separate hardening.
Missing recognized prefixes is not universal proof that a file has no VAE.
SVD header claims and provisional CogVideo/Mochi placement require representative
evidence before being treated as verified.

## Per-file family attribution

Use this precedence:

1. Explicit per-file user family override.
2. File-specific source metadata or a curated file-identity mapping.
3. A clearly resolved single-family template, only when its evidence supports
   attribution of this particular file.
4. A meaningful known umbrella for the file.
5. No family, producing flat placement under the resolved role.

Keep raw source metadata separate from the selected browsing label. Record the
classification reason and applied alias so users can explain a placement.
Contradictory evidence must not be resolved by taking the first template label.
Retain a supported umbrella or abstain instead.

A composite template can contain unrelated model families. Do not assign all
files to one template-selected family, or create placements under every family
listed by that template. Shared encoders and VAEs also need per-file evidence;
consumption by a workflow does not establish their architecture or family.

Per-file overrides must identify the selected file within a template bundle.
A single-model download's override applies to that file. Bulk selection may
apply an override to explicitly selected files, but never implicitly to an entire
multi-model template.

### Rules that are not permitted

- Selecting the first family when template labels are ambiguous.
- Using longest-substring matching as general proof of per-file family.
- Guessing `Flux.1 D` for unresolved `Flux`.
- Guessing a Klein size when no file-specific evidence identifies it.
- Treating a vendor label as a model family without supporting evidence.
- Requiring the family to exist in CivitAI.

The former example `image_flux2` selecting `Flux.2 Dev` did not satisfy its own
substring rule: `flux2dev` is not contained in `imageflux2`. Remove that rule,
not merely the example. Filename inference is permitted only through explicit,
tested rules with documented evidence, not fuzzy matching.

## Project-owned labels and aliases

CivitAI and template names are inputs to a project-owned browsing namespace.
Aliases are explicit mappings, not a generic suffix-stripping algorithm.
Unknown labels pass through unchanged before safe directory-name handling.

A browsing group can deliberately contain distinct task or size variants.
Such grouping is an organizational choice, not proof of shared weights.
For example, the earlier WAN justification named different VAEs for 14B and 5B;
it cannot support the claim that all those variants use the same VAE.

The following are candidate default mappings to validate with source fixtures.
They illustrate the naming policy; they are not newly verified upstream claims.
Each comma-separated source value represents a separate exact alias, not a prefix rule.

| Source family labels | Project browsing label |
|---|---|
| `Wan2.2`, `Wan Video 2.2 T2V-A14B`, `Wan Video 2.2 I2V-A14B`, `Wan Video 2.2 TI2V-5B` | `Wan Video 2.2` |
| `Z-Image-Turbo` | `ZImageTurbo` |
| `LTX-2` | `LTXV2` |
| `LTX-2.3` | `LTXV 2.3` |
| `SD1.5` | `SD 1.5` |

Ship only validated defaults. Preserve `Flux`, `Flux.1`, `Flux.2 Klein`,
`Stable Audio`, and previously unknown names unless an explicit supported alias
or stronger per-file evidence applies. Do not map generic `Wan2.1` to a
size-specific `Wan Video 14B` label without evidence of that size.

An explicit family override selects the user's browsing label and takes
precedence over automatic source aliasing. Validate labels as path components.
If sanitization makes different labels target the same path, apply the normal
content-conflict rules; do not silently merge different files.

### Configuration lifecycle

- Populate verified default aliases in the user config during initialization.
- For an existing config without this section, initialize the section without
  replacing unrelated settings.
- Once populated, treat the aliases as a user-owned snapshot. Runtime behavior
  must not silently overlay newly released defaults.
- Provide an explicit merge command with a preview of additions and conflicts.
- Preserve user-edited mappings. Previously removed aliases must not reappear
  without explicit approval during the merge.
- Retain enough baseline information to distinguish a removed default from a
  newly introduced default.
- Validate alias configuration and reject ambiguous or cyclic mappings rather
  than choosing by iteration order.
- Apply alias changes to new placement resolutions only. Do not rewrite existing
  placements or already persisted resolved destinations.

Exact config keys, CLI syntax, and baseline representation belong to the
implementation design. The CLI must perform catalog operations through IPC.

## Placement identity and coalescing

A placement is one resolved destination containing verified content. Transfer
history is not an additional owner of that destination.

Before transfer, coalesce requests by source identity and resolved destination.
When a verified SHA-256 is available, also coalesce by hash and destination,
including across sources. The same source with a different role, family, or
filename can require a distinct placement.

Source identity must retain distinctions that affect requested content, such as
file selection and revision. A URL alone is not a universal content identity.
A filename alone is never sufficient.

Resolve and persist a stable destination. If role inspection or response headers
are needed, defer final placement coalescing until those inputs are resolved.
Do not claim a guessed path is final. The implementation must define one filename
policy for metadata names, URL names, and Content-Disposition, applied consistently
to existing-path checks, transfers, and content reuse.

Before reusing an occupied destination, verify that it contains the requested
content. A catalog hash or matching filename alone is not verification of current
bytes. If content differs or identity cannot be established, fail with a conflict.
Do not overwrite, invent a new filename, or silently accept the existing file.

Coalescing must work across retries and concurrent requests. Publication must not
overwrite a destination created by another task after an earlier existence check.
Duplicate requests return the existing placement identity, not competing owners.
Deleting that placement is one explicit operation; it does not invalidate distinct
placements containing the same bytes.

## Content reuse through reflinks

When verified matching content is available before transfer:

1. Resolve the requested destination, including any inference needed from the
   verified source file.
2. If that destination is already the matching placement, reuse it after verification.
3. If another file occupies the destination, apply the conflict rules above.
4. Otherwise, attempt a filesystem reflink from the verified content into a
   temporary file at the destination filesystem.
5. Verify and publish the independent placement without overwriting another file.
6. Write placement-local metadata and record its destination and content hash.

Attempt an actual clone operation for the source and destination pair. Filesystem
names alone do not establish support. The clone operation must not silently
fall back to a byte copy.

A reflink creates an independent file whose data blocks can initially be shared.
Deleting either placement must preserve the other. Modifying either must not
modify the other. There is no canonical first-writer owner to promote on deletion.

If cloning fails, fail the new placement and leave existing placements intact.
Clean up temporary output. Report the requested path and cloning error. Do not
fall back to a symlink, hard link, full copy, fresh download, or alternate path
for that content-reuse attempt.

Sidecars belong to their placement and must describe that placement's provenance.
Do not blindly copy the source placement's metadata. Report the actual requested
destination on success, and distinguish reflink reuse from a fresh transfer.

### Late discovery of duplicates

If no verified reusable content is known, perform a normal download and verify it.
If identical bytes are discovered only after completion, keep the independently
downloaded file. Concurrent transfers may therefore leave physical duplicates.
Do not replace a verified completed transfer with a mandatory clone operation.

This feature does not guarantee one physical copy, exact disk savings, or global
content deduplication. Reflinks govern reuse of known content, not reconciliation
of all independently downloaded files.

## Persistence, scanning, and deletion

Persist per-file family decisions, role provenance, resolved destinations, and
placement identity through the queue and daemon restart. An additive IPC field
alone is insufficient. Changes must cover catalog schema migration, insertion,
row decoding, job types, and resolution plumbing.

Keep source `base_model` metadata separate from the project browsing family.
Do not overload it with a canonical folder label and then use that label for
architecture-sensitive role inference.

Replace URL-only deduplication in the template picker and version-only enqueue
coalescing where they would discard distinct placements. Share content knowledge
without collapsing different destinations into one job-owned path.

Reflinks are regular files and represent distinct placements. The scanner must
not collapse their paths merely because hashes match, or create duplicate records
for an already tracked path. Existing symlink skipping remains appropriate;
this feature creates no symlinks. Changes to scanner role coverage are separate.

Deleting a placement removes only its file and placement-local sidecars. Preserve
other placements with the same hash. Existing legacy jobs may share one path;
account for those references before unlinking that path. Do not discard the only
catalog information needed to recover from an unlink failure.

## Existing placements and updates

Preserve established paths, including files the old heuristic misplaced.
Routine updates must not relocate them based on new aliases or routing rules.
A changed shared helper must not silently activate updater relocation.

Explicitly requested new placements use the new rules. Existing verified content
can serve as a reflink source without moving or converting its old placement.
No retroactive reflinks, automatic reclassification, or reorganization command
is included in this change.

## Acceptance tests

### Role authority and attribution

- A template-declared checkpoint stays in `checkpoints` with `model.*` and
  `vae.*` headers, even though the current inspector misses the VAE prefix.
- An explicit user role overrides a template role and header inference.
- GGUF routing does not override an explicit role.
- An inferred HF repository role is not treated as a template declaration.
- Undeclared WAN and Hunyuan Video checkpoints without detected VAE/CLIP follow
  the corrected `diffusion_models` inference.
- LTXV and Hunyuan3D exception fixtures stay in `checkpoints`.
- `Hunyuan 1` no longer matches the bare Hunyuan exception.
- Changing a browsing alias does not change architecture-sensitive role inference.
- Composite templates classify files separately; unrelated stage labels do not
  become the file's family.
- File-specific evidence outranks template-wide hints; unresolved conflicts
  preserve a supported umbrella or fall back flat.
- Unknown family labels survive without CivitAI or template membership.
- Unresolved `Flux` stays `Flux`; unspecified Klein size is not guessed.
- A per-file override affects only the selected file, including in template bundles.
- Classification reason and raw metadata survive queue persistence and restart.

### Aliases and configuration

- Validated aliases converge source labels into the configured browsing label.
- Unknown suffixes are preserved; no generic task, size, or resolution stripping occurs.
- Stable Audio is not silently mapped to ACE Audio.
- Initialization populates defaults without replacing unrelated config values.
- New releases do not silently change a populated alias snapshot.
- Merge preview exposes conflicts, preserves edits, and requires approval to
  restore a previously removed alias.
- Invalid alias definitions and unsafe path components are rejected.
- Sanitization collisions trigger content verification, not silent overwrite.

### Placement and storage

- Repeated source-and-destination requests coalesce before transfer.
- Verified hash-and-destination matches coalesce across sources.
- Same-source requests with different destinations remain distinct through the
  picker, IPC, catalog, and downloader.
- Same-path content is verified before reuse; mismatched or unverifiable content
  fails without changing the existing file.
- On a supported filesystem, a pre-transfer hash match at another path creates
  a reflink with equal bytes and an independent file identity.
- Deleting or modifying either reflink placement leaves the other intact.
- Clone failure leaves no successful placement or partial destination, and invokes
  no symlink, hard-link, full-copy, download, or alternate-path fallback.
- Placement-local metadata describes the new destination and requesting source.
- Late hash matches after verified transfers keep independent files successfully.
- Concurrent publication cannot overwrite conflicting content or create competing
  owners of one destination.
- Filename resolution is consistent across reuse and transfer, including differing
  metadata and Content-Disposition names.
- Scanner handling preserves distinct reflink placements and avoids duplicate
  records for the same path; existing symlinks remain skipped.
- Deletion protects legacy shared-path references and remains recoverable after
  an unlink failure.

### Preservation and evidence

- Existing destinations remain unchanged during routine updates and alias merges.
- Old queued jobs remain readable after migration; absent family information does
  not become a fabricated family or declaration.
- Header and alias defaults have representative fixtures and traceable evidence.
- Catalog measurements include both properties and MarkdownNote model links.
- Reflink integration tests report unsupported environments explicitly, rather
  than passing through a copy fallback. Failure-path tests remain deterministic.

## Non-goals

- Inferring model compatibility from browsing folders.
- Workflow-specific dependency collections.
- Guessing per-file families from composite-template membership.
- Universal filesystem support for content reuse.
- Symlink ownership, canonical-file promotion, or a managed content store.
- Global physical deduplication or reconciliation of late duplicates.
- Automatic migration of installed files.

No Rust implementation or runtime configuration changes were part of the
specification revision itself. The implementation followed separately and is
described below.

## Implementation notes

### Where each part lives

| Concern | Location |
|---|---|
| Role and family resolution | `src/placement.rs`, `resolve` |
| Checkpoint-placement exception | `src/placement.rs`, `keeps_checkpoint_placement` |
| Alias namespace and validation | `src/placement.rs`, `AliasTable` |
| Alias snapshot, merge preview and apply | `src/config.rs`, `ModelFamiliesConfig` |
| Clone-published placements and sidecars | `src/daemon/store.rs` |
| Evidence assembly from a job | `src/daemon/downloader.rs`, `placement_evidence` |
| Persistence and migration | `src/catalog/`, eight added columns |
| Per-file overrides over IPC | `src/ipc/protocol.rs`, `AddDownload` and `QueueItem` |
| Template family accumulation | `src/cli/mod.rs`, `queue_items_for` |
| Atomic publication | `src/daemon/store.rs`, `publish_verified_temp` |
| Bundled and standalone VAE detection | `src/safetensor.rs`, `inspect_components` |
| Unverifiable-download guard | `src/safetensor.rs`, `verify_parseable` |
| Catalog and filesystem reconciliation | `src/catalog/mod.rs`, `diagnose` and `repair` |

`resolve` is pure and runs twice per download: once before the transfer for a
provisional destination, once after inspection for the final one. The user-facing
entry points are `comfyui-dl add --model-type --family`,
`comfyui-dl families [--merge --apply --restore-removed]` and
`comfyui-dl doctor [--repair]`.

### Reconciling the catalog with the filesystem

Not part of the specification, which rules out global content reconciliation,
but catalog-to-disk consistency is a different question and the audit showed why
it needs an answer: 21 rows named files that no longer existed and several paths
were named by more than one row, so `redownload-missing --all` would have
re-fetched roughly 80 GiB. `comfyui-dl doctor` reports dangling rows, paths with
more than one row, and model files no row names. `--repair` collapses duplicates
onto the row that still carries the CivitAI identifiers, because losing those
loses update tracking, and drops a dangling row only when another row accounts
for the same content. A dangling row nothing else covers is kept and reported:
it is the only remaining record of that model.

### Rejecting a download that was never verified

When a source publishes no hash there is nothing to check the bytes against. The
audit found two files in `.safetensors` names that were HuggingFace HTML error
pages, 126 KB and 138 KB, both recorded as completed downloads and both offered
by ComfyUI as usable models. A download that could not be verified by hash is
now parsed as safetensors before it is published, and fails the job otherwise.
This also catches a transfer that stopped early.

### Where coalescing is enforced

Coalescing has no single function, because the inputs it keys on become known at
different times. §"Placement identity and coalescing" is satisfied in three
places:

| Stage | Key | Location |
|---|---|---|
| Enqueue | version, role, file selection | `Catalog::find_active_or_done_job_by_version` |
| Template selection | url and role, accumulating family labels | `queue_items_for` |
| Publication | resolved destination and verified hash | `publish_by_clone` |

A version alone is not content identity, so the enqueue key includes the file
selection: requesting one version as `model-fp16.safetensors` and again as
`model-fp8.safetensors` yields two placements, while a repeat of the same
version, role and file selection returns the existing job. An earlier revision
of this implementation keyed on version and role only and collapsed the two
variants into one job.

Publication uses `renameat2` with `RENAME_NOREPLACE`, so a destination is
created atomically and "publication must not overwrite a destination created by
another task after an earlier existence check" holds without a lock. A task that
loses the race finds the winner's file and applies the ordinary content rules:
identical bytes are reused, different bytes are a conflict. A plain `rename`
would have replaced the winner's file silently. The flag is Linux-specific and
there is deliberately no fallback to an overwriting rename.

### Evidence for the alias defaults

`DEFAULT_FAMILY_ALIASES` ships empty, because §"Project-owned labels and
aliases" requires shipping only validated defaults and its candidate table did
not survive checking. The cached ComfyUI template index (287 workflows,
173 distinct family labels, read 2026-09-17) attests `Wan2.2`, `Z-Image-Turbo`,
`LTX-2`, `LTX-2.3` and `SD1.5` as source labels, but does **not** contain
`Wan Video 2.2 T2V-A14B`, `Wan Video 2.2 I2V-A14B` or `Wan Video 2.2 TI2V-5B`.
Those three were the convergence the `Wan Video 2.2` alias existed to perform,
so without them the alias is a bare rename. The remaining candidates rename
attested labels while leaving their siblings (`LTX-0.9.5`, `LTX-2.5`,
`Wan2.1`, `Wan2.5`, `Z-Image`) untouched, which would split one family across
aliased and unaliased folders. The merge command offers defaults once a source
fixture justifies them.

The same reading confirmed two of the specification's claims and unsettled one:

- `Hunyuan Video` is attested with a space and `HunyuanVideo` is not, as
  §"Inferred checkpoint corrections" predicted. Folding handles both.
- `ACE-Step` and `Stable Audio` are separate attested labels, so no alias
  between them ships.
- `LTXV` is **not** attested in the template index; the attested labels are
  `LTX-0.9.5`, `LTX-2`, `LTX-2.3` and `LTX-2.5`. The `ltxv` exception prefix is
  retained because it targets CivitAI `base_model` values, which this index does
  not cover, but it has no fixture. Do not broaden it to `ltx` without one.

### Corrections made after running against a real models tree

A 194 GiB installation of 42 models was audited on 2026-09-17. Four changes came
out of it, each with a fixture taken from a file in that tree.

**`vae.*` counts as a bundled VAE.** §"Inferred checkpoint corrections" left this
open pending fixtures. `ltx-video-2b-v0.9.safetensors` supplies one: 908 tensors
carrying `model.diffusion_model.*` together with `vae.encoder.*`,
`vae.decoder.*` and `vae.per_channel_statistics`. Before this, the inspector saw
no VAE in that file and an inferred checkpoint of its shape was rerouted to
`diffusion_models`, which is where the audit found it.

**Sentinel labels are not families.** CivitAI answers `"Other"` for some models
and our own metadata writer records `"Unknown"` when it has nothing. Both
appeared in sidecars in that tree, and a `diffusion_models/Other/` folder had
been built from the first. `other`, `unknown`, `n/a`, `none` and `null` are now
read as the absence of a family, case- and space-insensitively, so placement
falls back to flat. An explicit user override is still honoured: someone who
asks for a folder named `Other` gets one.

**A file that is only an autoencoder goes to `vae/`.** This extends the
specification, which restricts header inspection to the
checkpoint-to-`diffusion_models` correction. Four files in that tree were
standalone VAEs filed under `checkpoints/` and `diffusion_models/`, where
ComfyUI's VAE loader could not see them at all, because the source reports the
type of the model it ships rather than of each file inside it. The rule is
`decoder.*` present with no diffusion-weight prefixes, and it applies only to an
undeclared role; a declared role is still never rerouted. Validated against all
40 model files in that tree with zero false positives. Requiring the decoder is
what separates a VAE from a text encoder: `t5xxl_fp16` and
`umt5_xxl_fp8_e4m3fn_scaled` carry `encoder.*` keys and no decoder, and an
earlier version of this analysis wrongly classified both as VAEs.

**One alias default now ships.** `Z-Image-Turbo → ZImageTurbo` is the single
candidate two independent sources support: the template index publishes
`Z-Image-Turbo`, while a CivitAI `base_model` field in that tree reports
`ZImageTurbo`, which is also the folder name already in use.

### Deviations from the text above

1. **An unusable family label behaves differently by origin.** The
   specification requires label validation but not what failure does. An
   explicit user family override that cannot be a path component fails the
   placement, matching the treatment of declared roles; an unusable label from
   a source or template is abstained from, giving flat placement, matching
   "otherwise use flat placement".
2. **Deleting a model is two steps.** `Catalog::delete_model` returns the paths
   a placement owns and keeps the row; `Catalog::forget_model` drops the row
   once the caller reports the files are gone. This satisfies "do not discard
   the only catalog information needed to recover from an unlink failure", and
   changed the contract a previous test asserted.
3. **Scanner and enqueue coalescing now key on the path or the role, not the
   version.** `register_existing` skips a path already in the catalog and
   accepts a new path even when its version is known elsewhere; `enqueue`
   coalesces only within one role. A previous test asserted the opposite
   ("duplicate version_id must return None") and was rewritten.

### Operational consequence of the no-fallback rule

"Clone failure: fail the new placement; no link, full-copy, or alternate-path
fallback" is implemented literally, including the prohibition on a fresh
download. On a filesystem that cannot clone (ext4 without reflink support,
tmpfs), any request for content already verified elsewhere therefore fails
instead of downloading it again. Reuse was exercised on btrfs: two placements of
a 319.77 MiB file reported 0 B exclusive and 319.77 MiB shared, and deleting one
left the other intact with its extents becoming exclusive.

# Model placement heuristics

Status: proposed (spec only — not yet implemented)

## Problem

A model file can arrive from two sources with different amounts of placement
information:

- **Template downloads** (HuggingFace) carry an explicit ComfyUI subdirectory in
  the workflow JSON (`nodes[].properties.models[].directory`, or a `**role**`
  heading in the "Model links" note). This is authored by ComfyUI and is exact.
- **CivitAI downloads** carry only a coarse `ModelType` (`Checkpoint`, `LORA`,
  `VAE`, …) plus a `base_model` string (`"Flux.1 D"`, `"SDXL 1.0"`, `"Wan2.2"`).
  The exact ComfyUI subdirectory has to be inferred.

Today both paths converge on the same routing logic in `downloader::download`,
including a post-download safetensors-header inspection that reroutes some
checkpoints to `diffusion_models`. That inference is a guess, and in at least one
family it disagrees with what ComfyUI's own templates do.

## Goal

Split responsibility by source, and use the template catalog as the reference
that corrects the CivitAI heuristics:

1. **Templates decide the role subdirectory.** For a template model, the role
   the template declares (`checkpoints`, `loras`, `diffusion_models`, `vae`,
   `text_encoders`, …) is authoritative. Place the file in
   `models_dir / <role>` with no further inference and no header inspection.
2. **CivitAI decides the family subfolder.** For a CivitAI model, keep placing
   it under `models_dir / <role> / <family>`, where `<role>` comes from the
   ComfyUI heuristics and `<family>` comes from `base_model`. Correct the
   role heuristics where the template catalog proves them wrong.

Templates carry no family level and must not gain one; CivitAI keeps its family
level. The two placement shapes stay distinct:

```text
template:  models_dir / <role> / <file>
civitai:   models_dir / <role> / <family> / <file>
```

## Ground truth from the template catalog

Measured from the 285 cached workflow templates
(`~/.cache/comfyui-downloader/templates/workflows`, catalog snapshot
2026-09-15). Reproduce with:

```sh
jq -r '.nodes[]?.properties?.models[]?.directory // empty' *.json \
  | sort | uniq -c | sort -rn
```

Distinct `directory` values and their frequency:

| directory | count | directory | count |
|---|---|---|---|
| `text_encoders` | 100 | `controlnet` | 5 |
| `diffusion_models` | 83 | `audio_encoders` | 3 |
| `vae` | 70 | `model_patches` | 2 |
| `loras` | 35 | `latent_upscale_models` | 2 |
| `checkpoints` | 29 | `geometry_estimation` | 2 |
| `clip_vision` | 14 | `background_removal` | 2 |
| | | `upscale_models` | 1 |
| | | `style_models` | 1 |
| | | `detection` | 1 |

Two facts that shape the design:

- **All 15 directory values are already handled** by `templates::normalize_role`.
  No new role mappings are required.
- **Every filename maps to exactly one directory across all templates.** There
  are zero role conflicts, so a filename→role table derived from the catalog is
  unambiguous. (Verification query in the appendix.)
- **Template directories are always single-level.** No template nests a file
  under a family/base-model level, confirming templates must keep the
  `<role>/<file>` shape.

## Heuristic correction the catalog reveals

The catalog contradicts the current `is_video_checkpoint` heuristic in
`src/daemon/downloader.rs`.

Today's reroute logic: a CivitAI `Checkpoint` with no bundled VAE/CLIP is moved
to `diffusion_models`, **unless** `is_video_checkpoint(base_model)` is true — in
which case it is kept in `checkpoints`. That exception currently matches
`ltxv`, `cogvideo`, `wan`, `mochi`, `hunyuanvideo`, and `hunyuan`.

What the templates actually do for those families:

| family (template filename) | template directory |
|---|---|
| `ltx-video-2b-v0.9*.safetensors` | `checkpoints` |
| `svd_xt.safetensors` | `checkpoints` |
| `hunyuan3d-*` | `checkpoints` |
| `wan2.1_*`, `wan2.2_*` | `diffusion_models` |
| `Wan2_2-Animate-14B_*` | `diffusion_models` |
| `hunyuan_video_t2v_720p_*` | `diffusion_models` |
| `hunyuanvideo1.5_*` | `diffusion_models` |

So the exception is **wrong for WAN and HunyuanVideo**: templates place those in
`diffusion_models`, but `is_video_checkpoint` keeps a CivitAI copy in
`checkpoints`. It is **right for LTXV** (an all-in-one checkpoint that bundles a
VAE). `cogvideo` and `mochi` do not appear in the current catalog, so their
placement is **unverified** here.

Correction:

- Remove `wan`, `hunyuanvideo`, and bare `hunyuan` from the keep-in-checkpoints
  exception so they follow the normal no-VAE/no-CLIP → `diffusion_models`
  reroute, matching the templates.
- Keep `ltxv`.
- Leave `cogvideo` and `mochi` in place but mark them unverified; confirm by
  inspecting a sample file's safetensors header (see "Optional: inspect model
  examples") before trusting them.

Note the scope: `is_video_checkpoint` only affects the **CivitAI** path.
Template files carry an explicit directory and never reach this code once the
design below lands. The catalog is used here purely as the reference that says
what the correct CivitAI answer should be.

## Design

### Template path

In `downloader::download`, detect a template/HuggingFace job (the job already
carries `model_type` from the template's role via `AddDownloads`). When the role
is known:

- Place at `models_dir / <role> / <file>`.
- Do not append a family level.
- Do not run the safetensors-header reroute. The template's `directory` is
  already the final answer; header inspection can only introduce disagreement.

The role resolution order for a template file stays:
`job.model_type` (template `directory`) → `normalize_role(repo path segment)` →
`"other"`. This is unchanged from `resolve_huggingface`; the only change is
suppressing the checkpoint reroute for this path.

### CivitAI path

Unchanged in shape:

- Role from `ModelType::models_subdir`.
- Family subfolder from `sanitize_dir_name(base_model)`.
- Post-download reroute for `checkpoints` stays, with the corrected
  `is_video_checkpoint` list above.

### Where the split happens

`resolve_version` already distinguishes the two sources
(`resolve_huggingface` vs the CivitAI arms). Carry a source/`is_template` marker
on `VersionResolution` (or reuse the existing "HuggingFace" branch) so
`download` can:

- Skip the family-level `join(base_model)` for template files (templates have no
  `base_model`; this is already effectively true, but make it explicit).
- Gate the `if model_type_str == "checkpoints"` reroute block on
  `!is_template`.

## Non-goals

- No new role names or `normalize_role` mappings — the catalog needs none.
- No change to the family subfolder scheme for CivitAI.
- No retroactive move of already-placed files. Existing files keep their current
  location; this changes placement for new downloads only.
- No change to content-addressed dedup; a template file that is byte-identical to
  an existing CivitAI file still deduplicates to the existing path regardless of
  which role directory the template would have chosen.

## Interaction with dedup

Placement runs before the download, dedup by SHA-256 runs before the transfer.
When a template file deduplicates onto an existing CivitAI file, the reused path
keeps the CivitAI `<role>/<family>/<file>` shape. That is acceptable: the file
exists once, and the goal is to avoid a second copy, not to enforce two
locations. This is called out because it means a template's declared directory is
not guaranteed on a dedup hit — the existing file's directory wins.

## Test plan

- **Template role placement**: a job with template role `diffusion_models` lands
  at `models_dir/diffusion_models/<file>` with no family level and no reroute,
  even for a no-VAE/no-CLIP safetensors file.
- **Template checkpoint stays put**: a template role `checkpoints` file is not
  rerouted to `diffusion_models` by header inspection.
- **CivitAI WAN routes to diffusion_models**: a CivitAI `Checkpoint` with
  `base_model = "Wan2.2"` and no bundled VAE/CLIP lands in
  `diffusion_models/Wan2.2/…` (regression test for the corrected heuristic).
- **CivitAI LTXV stays in checkpoints**: a CivitAI `Checkpoint` with
  `base_model = "LTXV"` stays in `checkpoints/LTXV/…`.
- **`is_video_checkpoint` unit tests**: update to assert `wan`/`hunyuanvideo` are
  no longer treated as keep-in-checkpoints, `ltxv` still is.

## Optional: inspect model examples

For families the catalog does not cover (`cogvideo`, `mochi`), confirm placement
empirically before trusting the heuristic:

1. Download one small representative checkpoint for the family.
2. Read its safetensors header with `safetensor::inspect_components`.
3. If it bundles a VAE (`first_stage_model.*`), it is an all-in-one checkpoint →
   `checkpoints`. If not, it is a bare diffusion transformer →
   `diffusion_models`.

This is the same signal the reroute uses; running it once per unverified family
turns a guess into a checked fact.

## Appendix: verification queries

Run inside `~/.cache/comfyui-downloader/templates/workflows`.

Filename → role conflicts (expected: empty):

```sh
jq -r '.nodes[]?.properties?.models[]?
  | select(.directory != null)
  | [(.name // (.url|split("/")|last|split("?")|first)), .directory] | @tsv' *.json \
  | sort -u \
  | awk -F'\t' '{c[$1]=c[$1]" "$2} END{for(f in c){n=split(c[f],a," "); if(n>1) print f" =>"c[f]}}'
```

Files each family places in `checkpoints` vs `diffusion_models`:

```sh
jq -r '.nodes[]?.properties?.models[]?
  | select(.directory=="checkpoints" or .directory=="diffusion_models")
  | [.directory, (.name // (.url|split("/")|last|split("?")|first))] | @tsv' *.json \
  | grep -iE 'ltx|cogvideo|mochi|wan|hunyuan|svd' | sort -u
```

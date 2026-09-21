# comfyui-downloader TODO

## Bugs found while implementing the model-placement heuristics

Unrelated to that specification. Each was observed on 2026-09-17 against a
daemon running on an isolated XDG root; the cause of the first two was not
traced past the observation.

- [ ] Preview images take their extension from the URL, not the bytes. After
      downloading `Comfy-Org/z_image_turbo/split_files/vae/ae.safetensors`, the
      saved `ae.preview.jpeg` contained PNG data (`file` reports
      `PNG image data, 832 x 1216`). `preview_path_for_url` derives the suffix
      from the URL, so a `.jpeg` URL serving PNG bytes produces a mislabelled
      file. Deriving it from the response content type would fix it.
- [ ] The startup scanner writes a preview next to a model without updating
      that model's sidecar. For the same file, the scanner saved
      `ae.preview.jpeg` while `ae.metadata.json` still recorded
      `"preview_url": null` and `"from_civitai": false`, so the sidecar and the
      files on disk disagree about whether a preview exists.
- [ ] Deleting the last model in a family directory leaves the empty directory
      behind, so the browsing tree accumulates empty folders.
- [ ] Two files in the live models tree are HuggingFace HTML error pages saved
      under `.safetensors` names: `vae/qwen_image_vae.safetensors` (126948 bytes)
      and `loras/illustration-1.0-qwen-image.safetensors` (138009 bytes), both
      starting `<!doctype html>`. The first is a completed catalog entry with an
      empty `sha256`, so nothing verified it. ComfyUI lists both as usable
      models. New downloads are now guarded by `verify_parseable`, but these two
      files are still on disk and need deleting, and the catalog entry with them.
- [ ] A `.metadata.json` can exist at a path with no model file, recording
      `"size": 0`. Two such stubs were found at the original locations of models
      the old relocation had moved away. The likely writer was
      `update_metadata_file_path`, which has since been deleted with the
      relocation code, so this may already be fixed; worth confirming.
- [ ] A hand-written alias whose source label contains a dot must be quoted:
      `SD1.5 = "SD 1.5"` under `[model_families.aliases]` is read by TOML as a
      nested table and fails config loading with
      `invalid type: map, expected a string`, which does not point at the cause.
      The daemon writes these keys quoted, so only hand edits hit it. A clearer
      error, or accepting the dotted form, would help.

## Desktop Environment

- [ ] A [rofi](https://github.com/davatorium/rofi) menu to download a new model
- [ ] Better icons matching adawaita and/or breeze icons for the notifications

### Install and distribution

- [ ] Package as DEB and RPM files
- [ ] Github action
- [ ] Gitlab Pipeline
- [ ] Makefile to install, update or uninstall this program
- [ ] First public release 0.1

---

The following features are planned for the next release

## Integration with ComfyUI

- [ ] Using uv as runtime manager
- [ ] Manage instance and env vars as a systemd user unit file
- [ ] Query execution status from the CLI
- [ ] Execute saved workflows (only support single user), no parameter patching.

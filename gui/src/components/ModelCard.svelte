<script lang="ts">
  import { convertFileSrc } from "@tauri-apps/api/core";
  import type { EnrichedModel } from "$lib/types.ts";

  let { model }: { model: EnrichedModel } = $props();

  let previewSrc = $derived(
    model.preview_path ? convertFileSrc(model.preview_path) : null,
  );

  const TYPE_COLORS: Record<string, string> = {
    checkpoints: "#e94560",
    diffusion_models: "#c23616",
    loras: "#0097e6",
    vae: "#44bd32",
    controlnet: "#8c7ae6",
    embeddings: "#e1b12c",
    upscale_models: "#00cec9",
    other: "#636e72",
  };

  let badgeColor = $derived(TYPE_COLORS[model.model_type ?? ""] ?? "#636e72");

  function formatBytes(bytes: number | null): string {
    if (bytes == null || bytes === 0) return "";
    if (bytes >= 1_073_741_824) return (bytes / 1_073_741_824).toFixed(1) + " GB";
    if (bytes >= 1_048_576) return (bytes / 1_048_576).toFixed(0) + " MB";
    return (bytes / 1024).toFixed(0) + " KB";
  }

  function formatType(t: string | null): string {
    if (!t) return "unknown";
    return t.replace(/_/g, " ");
  }
</script>

<div class="card">
  <div class="preview">
    {#if previewSrc}
      <img src={previewSrc} alt={model.model_name ?? "Model"} loading="lazy" />
    {:else}
      <div class="placeholder">
        <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
          <rect x="3" y="3" width="18" height="18" rx="2" />
          <circle cx="8.5" cy="8.5" r="1.5" />
          <path d="m21 15-5-5L5 21" />
        </svg>
      </div>
    {/if}
  </div>
  <div class="info">
    <h3 class="name">{model.model_name ?? model.dest_path?.split("/").pop() ?? "Unknown"}</h3>
    <div class="meta">
      <span class="badge type" style="background: {badgeColor}">{formatType(model.model_type)}</span>
      {#if model.base_model}
        <span class="badge base">{model.base_model}</span>
      {/if}
    </div>
    {#if model.file_size}
      <span class="size">{formatBytes(model.file_size)}</span>
    {/if}
  </div>
</div>

<style>
  .card {
    background: #1e293b;
    border-radius: 12px;
    overflow: hidden;
    transition: transform 0.2s ease, box-shadow 0.2s ease;
    cursor: pointer;
  }
  .card:hover {
    transform: translateY(-4px);
    box-shadow: 0 12px 32px rgba(0, 0, 0, 0.4);
  }
  .preview {
    aspect-ratio: 4 / 3;
    overflow: hidden;
    background: #0f172a;
  }
  .preview img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
  .placeholder {
    width: 100%;
    height: 100%;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #475569;
  }
  .info {
    padding: 12px;
  }
  .name {
    font-size: 0.875rem;
    font-weight: 600;
    color: #e2e8f0;
    margin: 0 0 8px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meta {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    margin-bottom: 6px;
  }
  .badge {
    font-size: 0.675rem;
    padding: 2px 8px;
    border-radius: 999px;
    font-weight: 600;
    text-transform: capitalize;
  }
  .badge.type {
    color: #fff;
  }
  .badge.base {
    background: #334155;
    color: #94a3b8;
  }
  .size {
    font-size: 0.75rem;
    color: #64748b;
  }
</style>

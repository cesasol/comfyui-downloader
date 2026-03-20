<script lang="ts">
  import type { EnrichedModel } from "$lib/types.ts";

  let {
    models,
    selectedType = $bindable("all"),
    selectedBase = $bindable("all"),
  }: {
    models: EnrichedModel[];
    selectedType: string;
    selectedBase: string;
  } = $props();

  const TYPE_LABELS: Record<string, string> = {
    checkpoints: "Checkpoints",
    diffusion_models: "Diffusion Models",
    loras: "LoRA",
    vae: "VAE",
    controlnet: "ControlNet",
    embeddings: "Embeddings",
    upscale_models: "Upscalers",
    other: "Other",
  };

  let typeCounts = $derived.by(() => {
    const counts: Record<string, number> = {};
    for (const m of models) {
      const t = m.model_type ?? "other";
      counts[t] = (counts[t] ?? 0) + 1;
    }
    return counts;
  });

  let baseCounts = $derived.by(() => {
    const counts: Record<string, number> = {};
    const filtered =
      selectedType === "all"
        ? models
        : models.filter((m) => (m.model_type ?? "other") === selectedType);
    for (const m of filtered) {
      const b = m.base_model ?? "Unknown";
      counts[b] = (counts[b] ?? 0) + 1;
    }
    return counts;
  });

  let sortedTypes = $derived(
    Object.entries(typeCounts).sort((a, b) => b[1] - a[1]),
  );

  let sortedBases = $derived(
    Object.entries(baseCounts).sort((a, b) => b[1] - a[1]),
  );

  function selectType(type: string) {
    selectedType = type;
    selectedBase = "all";
  }
</script>

<aside class="sidebar">
  <div class="header">
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
      <path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z" />
    </svg>
    <h1>Model Gallery</h1>
  </div>

  <nav>
    <div class="section-label">Type</div>
    <button
      class="nav-item"
      class:active={selectedType === "all"}
      onclick={() => selectType("all")}
    >
      <span>All Models</span>
      <span class="count">{models.length}</span>
    </button>
    {#each sortedTypes as [type, count]}
      <button
        class="nav-item"
        class:active={selectedType === type}
        onclick={() => selectType(type)}
      >
        <span>{TYPE_LABELS[type] ?? type}</span>
        <span class="count">{count}</span>
      </button>
    {/each}

    {#if sortedBases.length > 0}
      <div class="section-label">Base Model</div>
      <button
        class="nav-item"
        class:active={selectedBase === "all"}
        onclick={() => (selectedBase = "all")}
      >
        <span>All</span>
        <span class="count">{sortedBases.reduce((s, [, c]) => s + c, 0)}</span>
      </button>
      {#each sortedBases as [base, count]}
        <button
          class="nav-item"
          class:active={selectedBase === base}
          onclick={() => (selectedBase = base)}
        >
          <span>{base}</span>
          <span class="count">{count}</span>
        </button>
      {/each}
    {/if}
  </nav>
</aside>

<style>
  .sidebar {
    width: 240px;
    min-width: 240px;
    background: #0f172a;
    border-right: 1px solid #1e293b;
    display: flex;
    flex-direction: column;
    overflow-y: auto;
    height: 100vh;
  }
  .header {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 20px 16px;
    border-bottom: 1px solid #1e293b;
    color: #e94560;
  }
  .header h1 {
    font-size: 1rem;
    font-weight: 700;
    color: #f1f5f9;
    margin: 0;
  }
  nav {
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .section-label {
    font-size: 0.675rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: #475569;
    padding: 12px 12px 4px;
  }
  .nav-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8px 12px;
    border-radius: 8px;
    border: none;
    background: transparent;
    color: #94a3b8;
    font-size: 0.8125rem;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
    text-align: left;
    width: 100%;
  }
  .nav-item:hover {
    background: #1e293b;
    color: #e2e8f0;
  }
  .nav-item.active {
    background: #1e293b;
    color: #e94560;
    font-weight: 600;
  }
  .count {
    font-size: 0.75rem;
    color: #475569;
    min-width: 24px;
    text-align: right;
  }
  .nav-item.active .count {
    color: #e94560;
  }
</style>

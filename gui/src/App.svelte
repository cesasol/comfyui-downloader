<script lang="ts">
  import { listModels } from "$lib/api.ts";
  import type { EnrichedModel } from "$lib/types.ts";
  import Sidebar from "./components/Sidebar.svelte";
  import Gallery from "./components/Gallery.svelte";

  let allModels: EnrichedModel[] = $state([]);
  let loading = $state(true);
  let error: string | null = $state(null);
  let selectedType = $state("all");
  let selectedBase = $state("all");

  let filteredModels = $derived.by(() => {
    let result = allModels;
    if (selectedType !== "all") {
      result = result.filter((m) => (m.model_type ?? "other") === selectedType);
    }
    if (selectedBase !== "all") {
      result = result.filter((m) => (m.base_model ?? "Unknown") === selectedBase);
    }
    return result;
  });

  async function loadModels() {
    loading = true;
    error = null;
    try {
      allModels = await listModels();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    loadModels();
  });
</script>

<div class="app">
  <Sidebar
    models={allModels}
    bind:selectedType
    bind:selectedBase
  />
  <Gallery
    models={filteredModels}
    {loading}
    {error}
  />
</div>

<style>
  :global(*) {
    box-sizing: border-box;
    margin: 0;
    padding: 0;
  }
  :global(body) {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: #0c0f1a;
    color: #e2e8f0;
    overflow: hidden;
  }
  .app {
    display: flex;
    height: 100vh;
    width: 100vw;
  }
</style>

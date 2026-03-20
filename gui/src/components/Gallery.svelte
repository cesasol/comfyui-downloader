<script lang="ts">
  import type { EnrichedModel } from "$lib/types.ts";
  import ModelCard from "./ModelCard.svelte";

  let {
    models,
    loading,
    error,
  }: {
    models: EnrichedModel[];
    loading: boolean;
    error: string | null;
  } = $props();

  let search = $state("");

  let filtered = $derived(
    search.trim() === ""
      ? models
      : models.filter((m) =>
          (m.model_name ?? "").toLowerCase().includes(search.toLowerCase()),
        ),
  );
</script>

<main class="gallery">
  <div class="toolbar">
    <div class="search-wrapper">
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
        <circle cx="11" cy="11" r="8" />
        <path d="m21 21-4.35-4.35" />
      </svg>
      <input
        type="text"
        placeholder="Search models..."
        bind:value={search}
      />
    </div>
    <span class="result-count">{filtered.length} model{filtered.length !== 1 ? "s" : ""}</span>
  </div>

  {#if loading}
    <div class="state">
      <div class="spinner"></div>
      <p>Loading models...</p>
    </div>
  {:else if error}
    <div class="state error-state">
      <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
        <circle cx="12" cy="12" r="10" />
        <path d="m15 9-6 6M9 9l6 6" />
      </svg>
      <p>{error}</p>
      <span class="hint">Make sure the daemon is running</span>
    </div>
  {:else if filtered.length === 0}
    <div class="state">
      <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
        <rect x="3" y="3" width="18" height="18" rx="2" />
        <circle cx="8.5" cy="8.5" r="1.5" />
        <path d="m21 15-5-5L5 21" />
      </svg>
      <p>{search ? "No models match your search" : "No models found"}</p>
      {#if !search}
        <span class="hint">Download models using the CLI to see them here</span>
      {/if}
    </div>
  {:else}
    <div class="grid">
      {#each filtered as model (model.id)}
        <ModelCard {model} />
      {/each}
    </div>
  {/if}
</main>

<style>
  .gallery {
    flex: 1;
    overflow-y: auto;
    background: #0c0f1a;
    height: 100vh;
  }
  .toolbar {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 16px 24px;
    border-bottom: 1px solid #1e293b;
    position: sticky;
    top: 0;
    background: #0c0f1a;
    z-index: 10;
  }
  .search-wrapper {
    display: flex;
    align-items: center;
    gap: 8px;
    background: #1e293b;
    border-radius: 8px;
    padding: 8px 12px;
    flex: 1;
    max-width: 400px;
    color: #64748b;
  }
  .search-wrapper input {
    background: none;
    border: none;
    outline: none;
    color: #e2e8f0;
    font-size: 0.875rem;
    width: 100%;
  }
  .search-wrapper input::placeholder {
    color: #475569;
  }
  .result-count {
    font-size: 0.8125rem;
    color: #64748b;
    white-space: nowrap;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
    gap: 16px;
    padding: 24px;
  }
  .state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 80px 24px;
    color: #475569;
    gap: 12px;
    text-align: center;
  }
  .state p {
    font-size: 1rem;
    color: #64748b;
    margin: 0;
  }
  .hint {
    font-size: 0.8125rem;
    color: #334155;
  }
  .error-state {
    color: #e94560;
  }
  .error-state p {
    color: #e94560;
  }
  .spinner {
    width: 32px;
    height: 32px;
    border: 3px solid #1e293b;
    border-top-color: #e94560;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>

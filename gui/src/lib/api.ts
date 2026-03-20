import { invoke } from "@tauri-apps/api/core";
import type { EnrichedModel, DaemonStatus } from "./types.ts";

export async function listModels(): Promise<EnrichedModel[]> {
  return invoke<EnrichedModel[]>("list_models");
}

export async function getStatus(): Promise<DaemonStatus> {
  return invoke<DaemonStatus>("get_status");
}

export async function addDownload(
  url: string,
  modelType?: string,
): Promise<unknown> {
  return invoke("add_download", { url, modelType });
}

export async function deleteModel(id: string): Promise<unknown> {
  return invoke("delete_model", { id });
}

export async function cancelDownload(id: string): Promise<unknown> {
  return invoke("cancel_download", { id });
}

export async function checkUpdates(): Promise<unknown> {
  return invoke("check_updates");
}

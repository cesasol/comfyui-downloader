export interface EnrichedModel {
  id: string;
  url: string;
  model_id: number | null;
  version_id: number | null;
  model_type: string | null;
  dest_path: string | null;
  created_at: string;
  updated_at: string;
  model_name: string | null;
  base_model: string | null;
  preview_path: string | null;
  preview_nsfw_level: number | null;
  file_size: number | null;
  sha256: string | null;
}

export interface DaemonStatus {
  queued: number;
  active: ActiveDownload[];
  free_bytes: number;
}

export interface ActiveDownload {
  id: string;
  bytes_received: number;
  total_bytes: number | null;
}

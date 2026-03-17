use comfyui_downloader::config::Config;
use comfyui_downloader::ipc::{IpcClient, Request, Response};
use serde_json::Value;
use uuid::Uuid;

async fn send_request(req: &Request) -> Result<Value, String> {
    let config = Config::load().map_err(|e| format!("failed to load config: {e}"))?;
    let mut client = IpcClient::connect(&config.daemon.socket_path)
        .await
        .map_err(|e| format!("failed to connect to daemon: {e}"))?;
    let resp = client
        .send(req)
        .await
        .map_err(|e| format!("IPC request failed: {e}"))?;
    match resp {
        Response::Ok(data) => Ok(data),
        Response::Err { message } => Err(message),
    }
}

#[tauri::command]
async fn list_models() -> Result<Value, String> {
    send_request(&Request::ListModelsEnriched).await
}

#[tauri::command]
async fn get_status() -> Result<Value, String> {
    send_request(&Request::GetStatus).await
}

#[tauri::command]
async fn add_download(url: String, model_type: Option<String>) -> Result<Value, String> {
    send_request(&Request::AddDownload { url, model_type }).await
}

#[tauri::command]
async fn delete_model(id: String) -> Result<Value, String> {
    let uuid = Uuid::parse_str(&id).map_err(|e| format!("invalid UUID: {e}"))?;
    send_request(&Request::DeleteModel { id: uuid }).await
}

#[tauri::command]
async fn cancel_download(id: String) -> Result<Value, String> {
    let uuid = Uuid::parse_str(&id).map_err(|e| format!("invalid UUID: {e}"))?;
    send_request(&Request::Cancel { id: uuid }).await
}

#[tauri::command]
async fn check_updates() -> Result<Value, String> {
    send_request(&Request::CheckUpdates).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            list_models,
            get_status,
            add_download,
            delete_model,
            cancel_download,
            check_updates,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

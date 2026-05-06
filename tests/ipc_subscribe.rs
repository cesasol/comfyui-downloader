//! Integration test: client opens a Subscribe connection, daemon emits events,
//! client observes the corresponding snapshot frames.

use comfyui_downloader::catalog::Catalog;
use comfyui_downloader::daemon::events::{new_bus, Event, EventBus};
use comfyui_downloader::ipc::protocol::{Frame, Request};
use comfyui_downloader::ipc::IpcServer;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

#[tokio::test]
async fn subscribe_emits_initial_snapshot_then_responds_to_events() {
    let tmp = TempDir::new().unwrap();
    let socket_path = tmp.path().join("test.sock");
    let db_path = tmp.path().join("catalog.db");
    let models_dir = tmp.path().join("models");
    std::fs::create_dir_all(&models_dir).unwrap();

    let catalog = Arc::new(Mutex::new(Catalog::open(&db_path).unwrap()));
    let progress = Arc::new(Mutex::new(HashMap::new()));
    let bus: EventBus = new_bus();

    let server = IpcServer::bind(&socket_path).unwrap();

    // Spawn server in background (only the subscribe handler matters here).
    let cat_s = catalog.clone();
    let prog_s = progress.clone();
    let bus_s = bus.clone();
    let models_dir_s = models_dir.clone();
    tokio::spawn(async move {
        let _ = server
            .serve(
                |_req| async move {
                    comfyui_downloader::ipc::protocol::Response::ok(serde_json::Value::Null)
                },
                move |writer| {
                    let cat = cat_s.clone();
                    let prog = prog_s.clone();
                    let bus = bus_s.clone();
                    let mdir = models_dir_s.clone();
                    async move {
                        comfyui_downloader::daemon::run_subscribe_for_test(
                            writer, cat, prog, bus, mdir,
                        )
                        .await
                    }
                },
            )
            .await;
    });

    // Give the server a beat to bind.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Client side: open a connection, send Subscribe, read frames.
    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let mut req = serde_json::to_string(&Request::Subscribe).unwrap();
    req.push('\n');
    writer.write_all(req.as_bytes()).await.unwrap();

    // Frame 1: Subscribed
    let line = lines.next_line().await.unwrap().unwrap();
    let frame: Frame = serde_json::from_str(&line).unwrap();
    assert!(matches!(frame, Frame::Subscribed));

    // Frame 2: initial Snapshot
    let line = lines.next_line().await.unwrap().unwrap();
    let frame: Frame = serde_json::from_str(&line).unwrap();
    let snap1 = match frame {
        Frame::Snapshot(s) => s,
        _ => panic!("expected snapshot"),
    };
    assert_eq!(snap1.seq, 1);
    assert!(!snap1.catalog_dirty);
    assert!(!snap1.updates_dirty);

    // Fire an event -> expect a fresh snapshot with catalog_dirty.
    bus.send(Event::CatalogChanged).unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let frame: Frame = serde_json::from_str(&line).unwrap();
    let snap2 = match frame {
        Frame::Snapshot(s) => s,
        _ => panic!("expected snapshot"),
    };
    assert_eq!(snap2.seq, 2);
    assert!(snap2.catalog_dirty);

    // Subsequent unrelated event -> catalog_dirty should reset to false.
    bus.send(Event::QueueChanged).unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let frame: Frame = serde_json::from_str(&line).unwrap();
    let snap3 = match frame {
        Frame::Snapshot(s) => s,
        _ => panic!("expected snapshot"),
    };
    assert_eq!(snap3.seq, 3);
    assert!(!snap3.catalog_dirty);
}

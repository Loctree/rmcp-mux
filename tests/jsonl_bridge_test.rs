use std::time::Duration;

use rust_mux::ipc::IpcEvent;
use tempfile::tempdir;
use tokio::sync::broadcast;

#[tokio::test]
async fn broadcasts_spawn_updates_from_events_jsonl() {
    let home = tempdir().expect("tempdir");
    let control_plane = home.path().join("control_plane");
    tokio::fs::create_dir_all(&control_plane)
        .await
        .expect("control plane dir");
    tokio::fs::write(
        control_plane.join("events.jsonl"),
        concat!(
            "{\"ts\":\"2026-05-24T12:00:00Z\",\"run_id\":\"run-1\",\"kind\":\"spawn-update\",\"payload\":{\"agent\":\"codex\",\"skill\":\"implement\",\"mode\":\"wave\",\"state\":\"running\",\"session_id\":\"sess-1\",\"launcher_pid\":4242,\"transcript\":\"/tmp/run-1.log\"}}\n",
            "{\"ts\":\"2026-05-24T12:00:01Z\",\"run_id\":\"ignored\",\"kind\":\"state\",\"payload\":{}}\n",
            "{\"ts\":\"2026-05-24T12:00:02Z\",\"run_id\":\"run-2\",\"kind\":\"spawn-update\",\"payload\":{\"agent\":\"claude\",\"skill\":\"review\",\"state\":\"completed\",\"exit_code\":0,\"report\":\"/tmp/run-2.md\"}}\n",
        ),
    )
    .await
    .expect("write events");

    let (tx, mut rx) = broadcast::channel(8);
    let bridge = tokio::spawn(rust_mux::jsonl_bridge::run_jsonl_bridge(
        home.path().to_path_buf(),
        tx,
    ));

    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("first event timeout")
        .expect("first event");
    let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("second event timeout")
        .expect("second event");

    bridge.abort();

    match first {
        IpcEvent::SpawnUpdate {
            run_id,
            agent,
            skill,
            mode,
            state,
            session_id,
            launcher_pid,
            transcript,
            ..
        } => {
            assert_eq!(run_id, "run-1");
            assert_eq!(agent, "codex");
            assert_eq!(skill, "implement");
            assert_eq!(mode, "wave");
            assert_eq!(state, "running");
            assert_eq!(session_id.as_deref(), Some("sess-1"));
            assert_eq!(launcher_pid, Some(4242));
            assert_eq!(transcript.unwrap().to_string_lossy(), "/tmp/run-1.log");
        }
        other => panic!("expected SpawnUpdate, got {other:?}"),
    }

    match second {
        IpcEvent::SpawnUpdate {
            run_id,
            agent,
            skill,
            state,
            exit_code,
            report,
            ..
        } => {
            assert_eq!(run_id, "run-2");
            assert_eq!(agent, "claude");
            assert_eq!(skill, "review");
            assert_eq!(state, "completed");
            assert_eq!(exit_code, Some(0));
            assert_eq!(report.unwrap().to_string_lossy(), "/tmp/run-2.md");
        }
        other => panic!("expected SpawnUpdate, got {other:?}"),
    }
}

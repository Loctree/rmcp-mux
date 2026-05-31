use crate::ipc::command::{MuxControlCommand, MuxControlResponse, NonMuxEntry, VerifyResult};
use crate::ipc::server::MuxControlContext;
use crate::scan::scan_hosts;
use std::sync::Arc;

pub async fn handle_command(
    ctx: Arc<MuxControlContext>,
    cmd: MuxControlCommand,
) -> Result<MuxControlResponse, String> {
    match cmd {
        MuxControlCommand::GetStatus => {
            let active = ctx.state.lock().await.max_active_clients;
            let status = crate::state::snapshot_for_state(&*ctx.state.lock().await, active);
            Ok(MuxControlResponse::Status(Box::new(status)))
        }
        MuxControlCommand::Verify { client_kind } => handle_verify(client_kind).await,
        MuxControlCommand::RestartService { name: _ } => Ok(MuxControlResponse::Unimplemented),
        MuxControlCommand::Subscribe => {
            // Handled at the connection loop level
            Ok(MuxControlResponse::Ok)
        }
        MuxControlCommand::Unsubscribe => {
            // Handled at the connection loop level
            Ok(MuxControlResponse::Ok)
        }
        MuxControlCommand::RouteSnapshot => {
            // Placeholder for routes logic.
            Ok(MuxControlResponse::Routes(vec![]))
        }
        MuxControlCommand::ReloadConfig => Ok(MuxControlResponse::Unimplemented),
        MuxControlCommand::Shutdown { graceful: _ } => Ok(MuxControlResponse::Unimplemented),
    }
}

async fn handle_verify(
    kind: crate::ipc::command::ClientKind,
) -> Result<MuxControlResponse, String> {
    let client_type = match kind {
        crate::ipc::command::ClientKind::Claude => crate::scan::HostKind::Claude,
        crate::ipc::command::ClientKind::Codex => crate::scan::HostKind::Codex,
        crate::ipc::command::ClientKind::Gemini => crate::scan::HostKind::Gemini,
        crate::ipc::command::ClientKind::Junie => crate::scan::HostKind::Junie,
        crate::ipc::command::ClientKind::Generic { .. } => {
            return Ok(MuxControlResponse::VerifyResult(VerifyResult {
                ok: true,
                non_mux_servers: vec![],
            }));
        }
    };

    let all_servers = scan_hosts();
    let mut non_mux = Vec::new();

    for scan_res in all_servers {
        if scan_res.host.kind.as_label() != client_type.as_label() {
            continue;
        }

        for srv in scan_res.services {
            let cmd_str = format!("{} {}", srv.command, srv.args.join(" "));
            if !cmd_str.contains("rust-mux") && !cmd_str.contains("mux-agent") {
                non_mux.push(NonMuxEntry {
                    client: client_type.as_label().to_string(),
                    path: scan_res.host.path.to_string_lossy().into_owned(),
                    line: 0, // scan_res.host doesn't have line number
                    server_name: srv.name.clone(),
                });
            }
        }
    }

    Ok(MuxControlResponse::VerifyResult(VerifyResult {
        ok: non_mux.is_empty(),
        non_mux_servers: non_mux,
    }))
}

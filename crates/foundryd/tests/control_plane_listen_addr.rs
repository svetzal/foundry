//! Binary-level proof that foundryd honors a configured non-default listen
//! address and serves plaintext tonic traffic there.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use foundryd::proto::{StatusRequest, foundry_client::FoundryClient};
use tempfile::TempDir;

struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Best-effort: tests may already have observed normal child exit.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn reserve_non_default_port() -> u16 {
    loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        if port != 50051 {
            return port;
        }
    }
}

fn spawn_foundryd(home: &TempDir, listen_addr: &str) -> ChildGuard {
    let child = Command::new(env!("CARGO_BIN_EXE_foundryd"))
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("FOUNDRYD_LISTEN_ADDR", listen_addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn foundryd");
    ChildGuard { child }
}

#[tokio::test(flavor = "multi_thread")]
async fn foundryd_serves_status_from_configured_non_default_listen_addr() {
    let home = tempfile::tempdir().expect("tempdir for daemon home");
    let port = reserve_non_default_port();
    let listen_addr = format!("127.0.0.1:{port}");
    let _child = spawn_foundryd(&home, &listen_addr);
    let client_addr = format!("http://{listen_addr}");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match FoundryClient::connect(client_addr.clone()).await {
            Ok(mut client) => {
                client
                    .status(StatusRequest {
                        workflow_id: String::new(),
                    })
                    .await
                    .expect("status RPC must succeed on the configured listen addr");
                break;
            }
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(error) => {
                panic!("foundryd never became reachable on configured addr {client_addr}: {error}")
            }
        }
    }
}

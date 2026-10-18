//! Operational rechecks always go through the daemon; no owner decision is recorded.
use crate::{
    daemon::{connect_daemon_required, status_to_anyhow},
    proto, render,
};
use anyhow::{Context, Result};

#[derive(Clone, Copy)]
pub enum Scope {
    Campaign,
    Initiative,
}

pub async fn recover(
    addr: &str,
    offline: bool,
    scope: Scope,
    name: &str,
    recovery_id: &str,
    evidence: Vec<String>,
    as_json: bool,
) -> Result<String> {
    if offline {
        anyhow::bail!("recovery reassessment requires the daemon; remove --offline");
    }
    let inspection = match scope {
        Scope::Campaign => "foundry campaign show <name> --offline",
        Scope::Initiative => "foundry initiative show <name> --offline",
    };
    let mut client = connect_daemon_required(addr, inspection).await?;
    let request = proto::RecoverRequest {
        name: name.into(),
        recovery_id: recovery_id.into(),
        evidence,
    };
    let response = match scope {
        Scope::Campaign => client.recover_campaign(request).await,
        Scope::Initiative => client.recover_initiative(request).await,
    }.map_err(|status| {
        if status.code() == tonic::Code::Unimplemented {
            anyhow::anyhow!("foundryd does not support typed recovery; update the daemon. No decision or resume fallback was sent.")
        } else { status_to_anyhow(status) }
    })?.into_inner();
    response.recovery.as_ref().context("daemon returned no recovery state")?;
    if as_json {
        Ok(format!("{}\n", serde_json::to_string_pretty(&response)?))
    } else {
        Ok(render::recovery::rechecked(&response))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = tonic_prost_build::configure();
    for message in [
        ".foundry.StageProviders",
        ".foundry.Campaign",
        ".foundry.CampaignEscalation",
        ".foundry.InitiativeMember",
        ".foundry.InitiativeBudget",
        ".foundry.InitiativeBrief",
        ".foundry.InitiativeEvidence",
        ".foundry.InitiativeDecision",
        ".foundry.InitiativeQuestion",
        ".foundry.Initiative",
        ".foundry.FindingSource",
        ".foundry.FindingConsumption",
        ".foundry.Finding",
        ".foundry.AdvanceInitiativeResponse",
        ".foundry.CampaignWorkInFlight",
        ".foundry.DeriveSource",
        ".foundry.MemberHold",
        ".foundry.MemberMilestone",
        ".foundry.TaskDispatch",
        ".foundry.MemberReservation",
        ".foundry.SteeringLease",
        ".foundry.EtCloseProposal",
        ".foundry.AuditEvidence",
        ".foundry.SteeringRecord",
        ".foundry.DurableSteeringAction",
        ".foundry.InitiativeWorkAdmission",
    ] {
        builder = builder.type_attribute(
            message,
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        );
    }
    builder.compile_protos(&["../../proto/foundry.proto"], &["../../proto"])?;
    Ok(())
}

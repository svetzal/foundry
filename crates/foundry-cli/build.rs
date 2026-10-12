fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .type_attribute(
            ".foundry.StageProviders",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.Campaign",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.CampaignEscalation",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.InitiativeMember",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.InitiativeBudget",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.InitiativeBrief",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.InitiativeEvidence",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.InitiativeDecision",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.InitiativeQuestion",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.Initiative",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.FindingSource",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.FindingConsumption",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .type_attribute(
            ".foundry.Finding",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        )
        .compile_protos(&["../../proto/foundry.proto"], &["../../proto"])?;
    Ok(())
}

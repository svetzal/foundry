fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::var("OUT_DIR")?;
    tonic_prost_build::configure()
        .file_descriptor_set_path(format!("{out_dir}/foundry_descriptor.bin"))
        .compile_protos(&["../../proto/foundry.proto"], &["../../proto"])?;
    Ok(())
}

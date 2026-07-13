fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .compile_protos(&["../../proto/tymux/v1/tymux.proto"], &["../../proto"])?;
    Ok(())
}

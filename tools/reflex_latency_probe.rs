//! Compatibility wrapper: the installed `nav doctor` owns the live probe policy.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let code = withnav::app::doctor::command(withnav::app::DoctorArgs {
        offline: false,
        json: false,
        workspace: std::env::current_dir()?,
        layer: Some(withnav::app::doctor::DoctorLayer::Reflex),
    })
    .await?;
    std::process::exit(code);
}

//! Compatibility wrapper: the installed `nav doctor` owns the live probe policy.
//! Permission-corpus calibration lives in `permission_corpus_probe`.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let code = withnav::app::doctor::command(withnav::app::DoctorArgs {
        offline: false,
        json: false,
        workspace: std::env::current_dir()?,
        layer: Some(withnav::app::doctor::DoctorLayer::Instinct),
    })
    .await?;
    std::process::exit(code);
}

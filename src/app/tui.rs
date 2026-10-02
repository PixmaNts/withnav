//! CLI adapter for the terminal client; owns process configuration only.
use super::TuiArgs;
use super::EXIT_DONE;

pub async fn run(args: TuiArgs) -> anyhow::Result<i32> {
    let embedded = args.url.is_none();
    let token = args
        .url
        .as_ref()
        .and_then(|_| std::env::var("RUST_AGENT_SERVER_TOKEN").ok());
    crate::tui::run(crate::tui::TuiOptions {
        url: args.url.unwrap_or_default(),
        token,
        workspace: args.workspace,
        initial_model: args.model,
        initial_session: args.session,
        embedded,
    })
    .await?;
    Ok(EXIT_DONE)
}

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    slack_mcp::server::run().await
}

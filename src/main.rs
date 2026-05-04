use tower_lsp::{LspService, Server};

mod backend;
mod config;
mod convert;
mod git;
mod gitlab;
mod server;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "gitlab_mr_lsp=debug".to_owned()),
        )
        .with_writer(std::io::stderr) // MUST be stderr — stdout is the LSP wire
        .with_ansi(false)             // Helix log viewer doesn't render ANSI codes
        .init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(server::Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

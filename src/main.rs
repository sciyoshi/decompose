#[tokio::main]
async fn main() -> std::process::ExitCode {
    let code = decompose::run_cli_from(
        std::env::args_os(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
    .await;
    std::process::ExitCode::from(code)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut out = std::io::stdout();
    let code = match ciburst::run(args, &mut out).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    };
    std::process::exit(code);
}

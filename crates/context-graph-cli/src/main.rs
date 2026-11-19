mod cli;
mod collect;
mod hooks;
mod process;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::dispatch(&args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error}");
            let code = match error.kind {
                context_graph_core::ErrorKind::InvalidInput => 2,
                context_graph_core::ErrorKind::Policy
                | context_graph_core::ErrorKind::Unauthorized => 3,
                _ => 1,
            };
            std::process::exit(code);
        }
    }
}

mod cli;
mod tui;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

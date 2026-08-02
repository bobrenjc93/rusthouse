mod cli;

fn main() {
    if let Err(error) = cli::run() {
        eprintln!("rusthouse: {error}");
        std::process::exit(1);
    }
}

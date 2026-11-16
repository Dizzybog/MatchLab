//! Trusted Community Learning workflow steps. See `matchlab-community help`.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = matchlab_community::cli::main(args) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

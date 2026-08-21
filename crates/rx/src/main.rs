fn main() {
    if let Err(error) = rx::run(std::env::args()) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

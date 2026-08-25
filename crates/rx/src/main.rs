fn main() {
    if let Err(error) = rx::run(std::env::args_os()) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

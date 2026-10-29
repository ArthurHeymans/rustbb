fn main() {
    std::process::exit(rbb_init::main_entry(
        &std::env::args().skip(1).collect::<Vec<_>>(),
    ));
}

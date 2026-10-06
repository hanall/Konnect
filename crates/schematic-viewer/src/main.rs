fn main() {
    if let Err(error) = schematic_viewer::run_from_env() {
        eprintln!("Schematic viewer failed: {error}");
        std::process::exit(1);
    }
}

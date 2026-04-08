fn main() {
    let program_name = std::env::args()
        .next()
        .and_then(|arg| {
            std::path::Path::new(&arg)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "afs-tests".to_string());
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(afs_tests::run_cli_named(&program_name, &args));
}

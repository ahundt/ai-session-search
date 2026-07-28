fn main() {
    match ai_session_search::run_cli_from(std::env::args_os()) {
        Ok(0) => {}
        Ok(exit_code) => std::process::exit(exit_code),
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
                    || cause
                        .downcast_ref::<serde_json::Error>()
                        .is_some_and(|error| {
                            error.io_error_kind() == Some(std::io::ErrorKind::BrokenPipe)
                        })
            }) => {}
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}

fn main() {
    #[cfg(unix)]
    // SAFETY: restoring SIGPIPE's process-wide default installs no Rust callback and retains no
    // pointer. Rust otherwise ignores SIGPIPE, which turns ordinary short-reader pipelines into
    // noisy stdout panics; native Unix CLIs conventionally terminate when the reader is done.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    match ai_session_search::run_cli_from(std::env::args_os()) {
        Ok(0) => {}
        Ok(exit_code) => std::process::exit(exit_code),
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
            }) => {}
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}

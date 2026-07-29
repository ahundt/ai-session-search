fn main() {
    match ai_session_search::run_cli_from(std::env::args_os()) {
        Ok(0) => {}
        Ok(exit_code) => std::process::exit(exit_code),
        Err(error) if ai_session_search::is_broken_pipe_error(&error) => {}
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}

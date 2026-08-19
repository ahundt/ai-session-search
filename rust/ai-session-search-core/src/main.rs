// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

fn main() {
    match ai_session_search::run_cli_from(std::env::args_os()) {
        Ok(0) => {}
        Ok(exit_code) => std::process::exit(exit_code),
        Err(error) if ai_session_search::is_broken_pipe_error(&error) => {}
        Err(error) => {
            // Formatting lives in the library so what this prints is what the Python binding
            // raises, and so the exact text a reader sees has a test.
            eprintln!(
                "error: {}",
                ai_session_search::error_message_with_recovery(&error)
            );
            std::process::exit(1);
        }
    }
}

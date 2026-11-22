use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [journal, socket] = args.as_slice() else {
        eprintln!("usage: eon-daemon <kernel-journal-dir> <socket-path>");
        return ExitCode::from(2);
    };
    match eon_daemon::run(Path::new(journal), Path::new(socket)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("eon-daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

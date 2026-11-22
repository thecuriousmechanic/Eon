use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

fn watch(dir: &Path, image: Option<&Path>) -> ExitCode {
    match eon_cli::run_watch_with_image(dir, Duration::from_millis(250), image) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("eon: {err}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    match args.as_slice() {
        [_, cmd, dir] if cmd == "status" => match eon_cli::read_status(Path::new(dir)) {
            Ok(status) => {
                print!("{}", eon_cli::render_text(&status));
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("eon: {err}");
                ExitCode::FAILURE
            }
        },
        [_, cmd, dir] if cmd == "watch" => watch(Path::new(dir), None),
        [_, cmd, dir, opt, image] if cmd == "watch" && opt == "--image" => {
            watch(Path::new(dir), Some(Path::new(image)))
        }
        _ => {
            eprintln!(
                "usage: eon status <journal-dir>\n       eon watch <journal-dir> [--image <PNG-or-JPEG-file>]"
            );
            ExitCode::from(2)
        }
    }
}

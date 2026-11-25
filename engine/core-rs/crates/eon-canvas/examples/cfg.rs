//! `cargo run --locked -p eon-canvas --example cfg < function.cfg`
//! Redirect one regular file of GCC successor records, not a pipe or a program.

use eon_canvas::cfg::{CfgView, MAX_BYTES};
use std::{error::Error, fs::File, io};

fn selected_file() -> io::Result<File> {
    // Use the already-open handle. There is no pathname to reopen or swap to a FIFO.
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        Ok(File::from(io::stdin().as_fd().try_clone_to_owned()?))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        Ok(File::from(io::stdin().as_handle().try_clone_to_owned()?))
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let usage = "usage: cfg < function.cfg (redirect one regular file of GCC successor records)";
    if std::env::args_os().nth(1).is_some() {
        return Err(usage.into());
    }
    let input = selected_file()?;
    let metadata = input.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(usage.into());
    }
    if metadata.len() > MAX_BYTES as u64 {
        return Err("CFG exceeds 16 KiB".into());
    }
    let graph = CfgView::read(input)?;
    Ok(eframe::run_native(
        "EON control-flow graph",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Wgpu,
            persist_window: false,
            ..Default::default()
        },
        Box::new(move |_| Ok(Box::new(graph))),
    )?)
}

use std::net::SocketAddr;
use std::path::PathBuf;

use eon_voice::asr::ZipformerAsr;
use eon_voice::tts::KokoroTts;
use eon_web::{AppState, router};

const DEFAULT_ADDR: &str = "127.0.0.1:8787";

#[derive(Debug, PartialEq)]
struct Config {
    journal: PathBuf,
    addr: SocketAddr,
    tts: Option<(PathBuf, PathBuf)>,
    asr_dir: Option<PathBuf>,
    memory: Option<PathBuf>,
    daemon_socket: Option<PathBuf>,
}

fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Config, String> {
    let mut journal = None;
    let mut addr = DEFAULT_ADDR.to_owned();
    let mut tts_model = None;
    let mut tts_voice = None;
    let mut asr_dir = None;
    let mut memory = None;
    let mut daemon_socket = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--journal" => journal = Some(PathBuf::from(value()?)),
            "--addr" => addr = value()?,
            "--tts-model" => tts_model = Some(PathBuf::from(value()?)),
            "--tts-voice" => tts_voice = Some(PathBuf::from(value()?)),
            "--asr-dir" => asr_dir = Some(PathBuf::from(value()?)),
            "--memory" => memory = Some(PathBuf::from(value()?)),
            "--daemon-socket" => daemon_socket = Some(PathBuf::from(value()?)),
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
    }
    let journal = journal.ok_or_else(|| format!("--journal is required\n{}", usage()))?;
    let addr: SocketAddr = addr
        .parse()
        .map_err(|e| format!("invalid --addr {addr:?}: {e}"))?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "refusing to bind {addr}: eon-web has no authentication and only serves loopback"
        ));
    }
    let tts = match (tts_model, tts_voice) {
        (Some(model), Some(voice)) => Some((model, voice)),
        (None, None) => None,
        _ => return Err("--tts-model and --tts-voice must be given together".to_owned()),
    };
    Ok(Config {
        journal,
        addr,
        tts,
        asr_dir,
        memory,
        daemon_socket,
    })
}

fn usage() -> String {
    "usage: eon-web --journal DIR [--addr 127.0.0.1:8787] \
     [--tts-model FILE --tts-voice FILE] [--asr-dir DIR] \
     [--memory FILE] [--daemon-socket PATH]"
        .to_owned()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = match parse_args(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    if let Some(memory) = &config.memory
        && !memory.is_file()
    {
        eprintln!("--memory {} does not exist", memory.display());
        std::process::exit(2);
    }

    let mut state = AppState::new(&config.journal)?;
    if let Some(memory) = &config.memory {
        eprintln!("opening memory {}", memory.display());
        state = state.with_recall(memory)?;
    }
    if let Some(socket) = &config.daemon_socket {
        state = state.with_daemon_socket(socket);
    }
    if let Some((model, voice)) = &config.tts {
        eprintln!("loading TTS from {}", model.display());
        state = state.with_tts(KokoroTts::load(model, voice)?);
    }
    if let Some(dir) = &config.asr_dir {
        eprintln!("loading ASR from {}", dir.display());
        state = state.with_asr(ZipformerAsr::load(dir)?);
    }

    let listener = tokio::net::TcpListener::bind(config.addr).await?;
    eprintln!(
        "eon-web on http://{} (journal {})",
        config.addr,
        config.journal.display()
    );
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn journal_is_required_and_defaults_to_loopback() {
        assert!(parse_args(args(&[])).is_err());
        let config = parse_args(args(&["--journal", "/tmp/j"])).unwrap();
        assert_eq!(config.addr, "127.0.0.1:8787".parse().unwrap());
        assert_eq!(config.tts, None);
        assert_eq!(config.asr_dir, None);
    }

    #[test]
    fn non_loopback_bind_is_refused() {
        let err = parse_args(args(&["--journal", "j", "--addr", "0.0.0.0:8787"])).unwrap_err();
        assert!(err.contains("refusing to bind"), "{err}");
    }

    #[test]
    fn tts_needs_both_model_and_voice() {
        let err = parse_args(args(&["--journal", "j", "--tts-model", "m.onnx"])).unwrap_err();
        assert!(err.contains("together"), "{err}");
    }

    #[test]
    fn memory_and_daemon_socket_are_optional_flags() {
        let config = parse_args(args(&[
            "--journal",
            "j",
            "--memory",
            "mem.loro",
            "--daemon-socket",
            "/run/eon.sock",
        ]))
        .unwrap();
        assert_eq!(config.memory, Some(PathBuf::from("mem.loro")));
        assert_eq!(config.daemon_socket, Some(PathBuf::from("/run/eon.sock")));
        let bare = parse_args(args(&["--journal", "j"])).unwrap();
        assert_eq!(bare.memory, None);
        assert_eq!(bare.daemon_socket, None);
    }
}

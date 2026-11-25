/// Fail before CPAL can ask ALSA for its default. This private configuration
/// has exactly one local Pulse server, no fallback devices or host includes.
pub fn require_null_sink() -> String {
    assert_eq!(std::env::var("EON_VOICE_TEST_VIRTUAL_MONITOR").as_deref(), Ok("1"));
    let server = std::env::var("PULSE_SERVER").expect("missing private server");
    let socket = server
        .strip_prefix("unix:/")
        .expect("expected an absolute local socket");
    assert!(!socket.contains(['"', '\\', '\n', '\r']));
    let conf =
        std::fs::read_to_string(std::env::var_os("ALSA_CONFIG_PATH").expect("missing private ALSA config")).unwrap();
    assert_eq!(
        conf,
        format!("pcm.!default {{ type pulse server \"{server}\" }}\n"),
        "ALSA default must route exclusively to the private Pulse socket"
    );
    for (kind, env, name) in [
        ("sources", "PULSE_SOURCE", "eon_vad.monitor"),
        ("sinks", "PULSE_SINK", "eon_vad"),
    ] {
        assert_eq!(std::env::var(env).as_deref(), Ok(name));
        let list = std::process::Command::new("pactl")
            .args(["--server", &server, "list", "short", kind])
            .output()
            .unwrap();
        assert!(list.status.success(), "private server unavailable");
        let entries = String::from_utf8(list.stdout).unwrap();
        let names: Vec<_> = entries
            .lines()
            .map(|line| {
                let mut fields = line.split('\t').skip(1);
                (fields.next(), fields.next())
            })
            .collect();
        assert_eq!(
            names,
            [(Some(name), Some("module-null-sink.c"))],
            "unexpected {kind} on dedicated server"
        );
    }
    server
}

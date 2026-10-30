#![allow(clippy::unwrap_used)]

use super::*;

fn service(dir: &str) -> Service {
    Service {
        program: format!("{dir}/.cargo/bin/wye").into(),
        config: format!("{dir}/.wye/wye.toml").into(),
        log: format!("{dir}/.wye/usage.jsonl").into(),
        errors: format!("{dir}/.wye/wye.log").into(),
        stop_secs: 35,
    }
}

#[test]
fn a_definition_reads_back_as_written() {
    for manager in [Manager::Launchd, Manager::Systemd] {
        let written = service("/Users/a&b");
        let written = if manager == Manager::Systemd {
            service("/home/a%b")
        } else {
            written
        };
        let text = written.definition(manager).unwrap();
        assert_eq!(Service::parse(manager, &text), Some(written), "{text}");
    }
    let plist = service("/Users/a&b").definition(Manager::Launchd).unwrap();
    assert!(
        plist.contains("<string>/Users/a&amp;b/.wye/wye.toml</string>"),
        "{plist}"
    );
    assert!(plist.contains("<key>ExitTimeOut</key><integer>35</integer>"));
    let unit = service("/home/a%b").definition(Manager::Systemd).unwrap();
    assert!(
        unit.contains(
            "ExecStart=/home/a%%b/.cargo/bin/wye serve --config /home/a%%b/.wye/wye.toml\n"
        ),
        "{unit}"
    );
    assert!(unit.contains("StandardOutput=append:/home/a%%b/.wye/usage.jsonl\n"));
    assert!(unit.contains("StandardError=append:/home/a%%b/.wye/wye.log\n"));
}

#[test]
fn systemd_refuses_paths_it_would_split() {
    let error = service("/home/a b")
        .definition(Manager::Systemd)
        .unwrap_err();
    assert!(error.contains("spaces or quotes"), "{error}");
}

#[test]
fn a_definition_wye_did_not_write_is_left_alone() {
    let foreign = "<plist><dict><key>Label</key><string>dev.wye.gateway</string>\
                   <array><string>/usr/bin/wye</string><string>serve</string></array>\
                   </dict></plist>";
    assert_eq!(Service::parse(Manager::Launchd, foreign), None);
    assert_eq!(
        Service::parse(
            Manager::Systemd,
            "[Service]\nExecStart=/usr/bin/wye serve\n"
        ),
        None
    );
}

#[test]
fn the_plan_says_what_changes_and_how_to_undo_it() {
    let show = |p: &Path| p.display().to_string().replace("/Users/a", "~");
    let file = Manager::Launchd.file(Path::new("/Users/a"));
    let commands = start_commands(Manager::Launchd, "501", &file, true);
    let text = plan(
        Manager::Launchd,
        &service("/Users/a"),
        &file,
        true,
        &commands,
        &show,
    );
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("as a macOS LaunchAgent: launchd starts it now and at every login, as you,"),
        "{text}"
    );
    for line in [
        "  Replace  ~/Library/LaunchAgents/dev.wye.gateway.plist\n",
        "  Run      ~/.cargo/bin/wye serve --config ~/.wye/wye.toml\n",
        "  Log      ~/.wye/usage.jsonl (appended)\n",
        "  Errors   ~/.wye/wye.log (appended)\n",
        "  Then     launchctl bootout gui/501/dev.wye.gateway\n",
        "           launchctl bootstrap gui/501 ~/Library/LaunchAgents/dev.wye.gateway.plist\n",
        "Undo with `wye service uninstall`.",
    ] {
        assert!(text.contains(line), "{line}\n---\n{text}");
    }
    // Prose wraps at 80 columns; a command stays whole, so it can be copied.
    for line in text.lines().filter(|l| !l.starts_with("  ")) {
        assert!(line.len() <= crate::cli::args::WIDTH, "{line}");
    }
    assert_eq!(
        start_commands(Manager::Systemd, "1000", &file, false),
        [
            systemctl(&["daemon-reload"]),
            systemctl(&["enable", UNIT]),
            systemctl(&["restart", UNIT]),
        ]
    );
}

#[test]
fn secrets_from_the_environment_are_named() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n\
         [cache_diagnostics]\nhash_key_env = \"WYE_HASH_KEY\"\n",
    )
    .unwrap();
    assert_eq!(env_secrets(&config), ["ANTHROPIC_API_KEY", "WYE_HASH_KEY"]);
    let file = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [upstream]\nbase_url = \"https://api.anthropic.com\"\napi_key_file = \"anthropic.key\"\n",
    )
    .unwrap();
    assert!(env_secrets(&file).is_empty());
}

#[test]
fn the_gateway_is_asked_on_this_machine() {
    assert_eq!(
        local_url("0.0.0.0:8080".parse().unwrap()),
        "http://127.0.0.1:8080"
    );
    assert_eq!(local_url("[::]:9000".parse().unwrap()), "http://[::1]:9000");
    assert_eq!(
        local_url("127.0.0.1:8080".parse().unwrap()),
        "http://127.0.0.1:8080"
    );
}

#[test]
fn a_log_size_reads_as_people_write_it() {
    assert_eq!(file_size(934_719), "912 KB");
    assert_eq!(file_size(4_404_019), "4.2 MB");
    assert_eq!(file_size(512), "512 bytes");
}

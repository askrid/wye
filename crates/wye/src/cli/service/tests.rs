use super::*;

fn service(dir: &str) -> Service {
    Service {
        program: format!("{dir}/.cargo/bin/wye").into(),
        config: format!("{dir}/.wye/wye.toml").into(),
        usage_log: format!("{dir}/.wye/usage.jsonl").into(),
        messages: format!("{dir}/.wye/wye.log").into(),
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
    // The gateway writes and rotates the usage log itself; its stdout,
    // empty, goes to the message log with its stderr.
    assert!(
        plist.contains(
            "    <string>--config</string>\n    <string>/Users/a&amp;b/.wye/wye.toml</string>\n    \
             <string>--usage-log</string>\n    <string>/Users/a&amp;b/.wye/usage.jsonl</string>\n"
        ),
        "{plist}"
    );
    for key in ["StandardOutPath", "StandardErrorPath"] {
        assert!(
            plist.contains(&format!(
                "<key>{key}</key><string>/Users/a&amp;b/.wye/wye.log</string>\n"
            )),
            "{plist}"
        );
    }
    let unit = service("/home/a%b").definition(Manager::Systemd).unwrap();
    assert!(
        unit.contains(
            "ExecStart=/home/a%%b/.cargo/bin/wye serve --config /home/a%%b/.wye/wye.toml \
             --usage-log /home/a%%b/.wye/usage.jsonl\n"
        ),
        "{unit}"
    );
    assert!(unit.contains("StandardOutput=append:/home/a%%b/.wye/wye.log\n"));
    assert!(unit.contains("StandardError=append:/home/a%%b/.wye/wye.log\n"));
    // Room for a client and an upstream connection per max_connections.
    assert!(unit.contains("\nLimitNOFILE=65536\n"), "{unit}");
    for limits in ["SoftResourceLimits", "HardResourceLimits"] {
        assert!(
            plist.contains(&format!(
                "<key>{limits}</key>\n  <dict>\n    \
                 <key>NumberOfFiles</key><integer>65536</integer>\n  </dict>\n"
            )),
            "{plist}"
        );
    }
}

/// Only what `install` writes reads back: a definition in another form
/// is someone else's, which `install` offers to replace.
#[test]
fn a_definition_without_its_usage_log_flag_is_not_ours() {
    for manager in [Manager::Launchd, Manager::Systemd] {
        let text = service("/home/a").definition(manager).unwrap();
        let other = match manager {
            Manager::Launchd => text.replace(
                "    <string>--usage-log</string>\n    <string>/home/a/.wye/usage.jsonl</string>\n",
                "",
            ),
            Manager::Systemd => text.replace(" --usage-log /home/a/.wye/usage.jsonl", ""),
        };
        assert_ne!(other, text);
        assert_eq!(Service::parse(manager, &other), None, "{other}");
    }
}

#[test]
fn systemd_refuses_paths_it_would_split() {
    for odd in [
        "/home/a b",
        "/home/o'brien",
        "/home/a$b",
        "/home/a\"b",
        "/home/a\\b",
    ] {
        let error = service(odd).definition(Manager::Systemd).unwrap_err();
        assert!(error.contains("spaces, quotes, or $"), "{error}");
    }
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
        "  Replace   ~/Library/LaunchAgents/dev.wye.gateway.plist\n",
        "  Run       ~/.cargo/bin/wye serve --config ~/.wye/wye.toml\n",
        "  Usage log ~/.wye/usage.jsonl (appended)\n",
        "  Messages  ~/.wye/wye.log (appended)\n",
        "  Then      launchctl bootout gui/501/dev.wye.gateway\n",
        "            launchctl bootstrap gui/501 ~/Library/LaunchAgents/dev.wye.gateway.plist\n",
        "Undo with `wye service uninstall`.",
    ] {
        assert!(text.contains(line), "{line}\n---\n{text}");
    }
    // Prose wraps at 80 columns; a command stays whole, so it can be copied.
    for line in text.lines().filter(|l| !l.starts_with("  ")) {
        assert!(line.len() <= crate::cli::text::WIDTH, "{line}");
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
fn the_uninstall_plan_says_what_stops_goes_and_stays() {
    let show = |p: &Path| p.display().to_string().replace("/home/a", "~");
    let file = Manager::Systemd.file(Path::new("/home/a"));
    let text = uninstall_plan(
        &service("/home/a"),
        &file,
        &stop_commands(Manager::Systemd, "1000"),
        &vec![systemctl(&["daemon-reload"])],
        &show,
    );
    for line in [
        "  Stop      systemctl --user disable --now wye.service\n",
        "  Remove    ~/.config/systemd/user/wye.service\n",
        "  Then      systemctl --user daemon-reload\n",
        "  Keep      ~/.wye/wye.toml (the config)\n",
        "            ~/.wye/usage.jsonl (the usage log and its rotated files)\n",
        "            ~/.wye/wye.log (the message log)\n",
    ] {
        assert!(text.contains(line), "{line}\n---\n{text}");
    }
    for line in text.lines() {
        assert!(line.len() <= crate::cli::text::WIDTH, "{line}");
    }
}

#[test]
fn secrets_from_the_environment_are_named() {
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key_env = \"ANTHROPIC_API_KEY\"\n\
         [cache_diagnostics]\nhash_key_env = \"WYE_HASH_KEY\"\n",
    )
    .unwrap();
    assert_eq!(env_secrets(&config), ["ANTHROPIC_API_KEY", "WYE_HASH_KEY"]);
    let file = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\n\
         [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key_file = \"anthropic.key\"\n",
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

#[test]
fn loading_the_service_waits_out_the_old_gateway_shutting_down() {
    let every = Duration::from_millis(1);
    let mut left = 3;
    let busy = |left: &mut i32| {
        *left -= 1;
        if *left > 0 {
            Err("Bootstrap failed: 5: Input/output error".to_owned())
        } else {
            Ok(())
        }
    };
    assert_eq!(
        retry(|| busy(&mut left), Duration::from_secs(5), every),
        Ok(())
    );
    assert_eq!(left, 0, "kept trying until the old gateway had gone");
    let mut tries = 0;
    let never = || {
        tries += 1;
        Err("still busy".to_owned())
    };
    assert_eq!(
        retry(never, Duration::from_millis(5), every),
        Err("still busy".to_owned()),
        "a real failure still comes out"
    );
    assert!(tries > 1);
}

#[test]
fn a_restart_brings_the_stop_timeout_up_to_the_config() {
    let installed = service("/Users/a");
    let config = Config::from_toml_str(
        "[server]\nlisten = \"127.0.0.1:8080\"\ndrain_delay_secs = 10\nshutdown_grace_secs = 60\n\
         [[routes.claude.upstreams]]\nbase_url = \"https://api.anthropic.com\"\nprovider_key_env = \"K\"\n",
    )
    .unwrap();
    let refreshed = refreshed(&installed, &config);
    assert_eq!(refreshed.stop_secs, 75, "drain + grace + 5");
    assert_eq!(
        Service {
            stop_secs: installed.stop_secs,
            ..refreshed
        },
        installed,
        "the program, config, and logs stay as installed"
    );
}

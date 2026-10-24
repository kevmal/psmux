use super::normalize_window_target_args;

#[test]
fn every_target_spelling_retains_the_explicit_target() {
    for args in [
        vec!["-t", "probe:%bad"], vec!["-tprobe:%bad"], vec!["-t=probe:%bad"],
        vec!["--target", "probe:%bad"], vec!["--target=probe:%bad"],
    ] {
        assert_eq!(normalize_window_target_args("kill-window", &args).unwrap(), ["-t", "probe:%bad"]);
    }
    for args in [vec!["-at", "probe:%bad"], vec!["-atprobe:%bad"], vec!["-at=probe:%bad"]] {
        assert_eq!(normalize_window_target_args("killw", &args).unwrap(), ["-a", "-t", "probe:%bad"]);
    }
}

#[test]
fn missing_empty_and_unknown_target_flags_are_errors() {
    for args in [vec!["-t"], vec!["-at"], vec!["--target"], vec!["--target="],
        vec!["-t="], vec!["-t", ""], vec!["--targeting=probe:%bad"],
        vec!["-1tprobe:%bad"], vec!["-_tprobe:%bad"], vec!["-Ztprobe:%bad"]] {
        assert!(normalize_window_target_args("kill-window", &args).is_err(), "{args:?}");
    }
}

#[test]
fn attached_sources_cannot_become_the_current_window() {
    for command in ["move-window", "swap-window", "link-window"] {
        for args in [vec!["-sprobe:%bad"], vec!["-s=probe:%bad"], vec!["--source=probe:%bad"]] {
            assert_eq!(normalize_window_target_args(command, &args).unwrap(), ["-s", "probe:%bad"]);
        }
        assert!(normalize_window_target_args(command, &["-s"]).is_err());
    }
}

#[test]
fn target_like_payloads_and_other_option_values_are_preserved() {
    assert_eq!(normalize_window_target_args("set-window-option", &["-tprobe:%1", "@x", "-target"]).unwrap(),
        ["-t", "probe:%1", "@x", "-target"]);
    assert_eq!(normalize_window_target_args("respawn-window", &["-c", "-target", "-tprobe:%1"]).unwrap(),
        ["-c", "-target", "-t", "probe:%1"]);
    assert_eq!(normalize_window_target_args("rename-window", &["--", "-target"]).unwrap(), ["--", "-target"]);
    assert_eq!(normalize_window_target_args("bind-key", &["x", "kill-window", "-tprobe:%1"]).unwrap(),
        ["x", "kill-window", "-tprobe:%1"]);
}

use xero_bot::commands::parse_commands;
use xero_bot::config::repository::*;

const REPO: &str = "example/project";
/// Parse one test document and require structural validity before inspecting its domains.
fn parse(text: &str) -> RepositoryConfig {
    RepositoryConfig::parse(text, REPO).unwrap()
}

/// Missing sections and legacy idle-only documents must retain every documented default.
#[test]
fn defaults_empty_partial_and_idle_only() {
    for text in [
        "",
        "# empty",
        "[idle_workflows]\nenabled=false",
        include_str!("../examples/idle-workflows.toml"),
        include_str!("../examples/repository-config.toml"),
    ] {
        let cfg = parse(text);
        let c = cfg.comments.unwrap();
        assert_eq!(c.ttl_days, 30);
        assert_eq!(CommandId::ALL.len(), 16);
        for id in CommandId::ALL {
            assert_eq!(c.mode(id), id.default_mode());
            assert_ne!(c.mode(id), ManualMode::Disabled);
        }
        assert!(cfg.events.unwrap().is_empty());
        assert!(cfg.paths.unwrap().rules.is_empty());
        assert!(cfg.idle.is_ok());
    }
    let c = parse("[command_triggers]\nreview={mode='disabled'}\n[command_sessions]\nttl_days=365")
        .comments
        .unwrap();
    assert_eq!(c.ttl_days, 365);
    assert_eq!(c.mode(CommandId::Review), ManualMode::Disabled);
    assert_eq!(c.mode(CommandId::Claim), ManualMode::NoMention);
    assert_eq!(c.mode(CommandId::Approve), ManualMode::AlwaysMention);
    assert_eq!(c.mode(CommandId::Help), ManualMode::MentionOnce);
}

/// Neither spelling, mention evidence, session state nor PR applicability may bypass disabled.
#[test]
fn every_alias_shares_its_canonical_disabled_policy_at_all_entrances() {
    for id in CommandId::ALL {
        for alias in id.aliases() {
            let cfg = parse(&format!(
                "[command_triggers]\n'{alias}'={{mode='disabled'}}"
            ));
            let c = cfg.comments.unwrap();
            for spelling in id.aliases() {
                assert_eq!(CommandId::from_name(spelling), Some(id));
                assert_eq!(
                    c.mode(CommandId::from_name(spelling).unwrap()),
                    ManualMode::Disabled
                );
            }
            for explicit in [true, false] {
                for session in [true, false] {
                    for is_pr in [true, false] {
                        assert_eq!(
                            c.gate(id, is_pr, explicit, session).unwrap_err().code,
                            ReasonCode::Disabled
                        );
                    }
                }
            }
        }
    }
}

/// The parser and policy registry must identify the same command and preserve approval targets.
#[test]
fn parser_and_config_agree_on_all_aliases_including_r_equals() {
    for id in CommandId::ALL {
        for alias in id.aliases() {
            let args = if *alias == "r=" {
                " @alice"
            } else {
                match id {
                    CommandId::Assign | CommandId::RequestReview | CommandId::Cc => " @alice",
                    CommandId::Label => " +triage",
                    _ => "",
                }
            };
            let output = parse_commands("bot", &format!("@bot {alias}{args}"));
            assert_eq!(output.commands.len(), 1, "{alias}: {output:?}");
            assert_eq!(output.commands[0].id(), id, "{alias}");
        }
    }
    for text in ["@bot r+ as @alice", "@bot r+ @alice", "@bot r= @alice"] {
        let out = parse_commands("bot", text);
        assert_eq!(
            out.commands
                .into_iter()
                .map(|c| c.command)
                .collect::<Vec<_>>(),
            vec![xero_bot::commands::Command::Approve {
                on_behalf_of: Some("alice".into())
            }],
            "{text}"
        );
    }
}

/// Ambiguous overrides, unknown commands and invalid modes/TTLs must disable only comments.
#[test]
fn alias_collisions_unknown_names_and_auto_are_domain_errors() {
    for text in [
        "claim={mode='disabled'}\ntake={mode='no_mention'}",
        "'r+'={mode='disabled'}\n'r='={mode='no_mention'}",
        "review={mode='auto'}",
        "help={mode='typo'}",
        "unknown={mode='disabled'}",
    ] {
        let cfg = parse(&format!("[command_triggers]\n{text}"));
        assert_eq!(cfg.comments.unwrap_err().code, ReasonCode::InvalidComments);
        assert!(cfg.events.is_ok());
        assert!(cfg.paths.is_ok());
        assert!(cfg.idle.is_ok());
    }
    for days in [0, -1, 366] {
        assert!(parse(&format!("[command_sessions]\nttl_days={days}"))
            .comments
            .is_err());
    }
    for days in [1, 365] {
        assert!(parse(&format!("[command_sessions]\nttl_days={days}"))
            .comments
            .is_ok());
    }
}

/// Structural mistakes must never produce a usable partial document or default policy.
#[test]
fn syntax_types_unknown_fields_and_duplicates_reject_the_document() {
    for text in [
        "[",
        "unknown=true",
        "command_triggers=5",
        "[command_sessions]\nttl_days='30'",
        "[command_sessions]\nunknown=30",
        "[command_triggers]\nreview={mode='disabled',typo=true}",
        "[command_triggers]\nreview={mode='disabled'}\nreview={mode='always_mention'}",
        "[idle_workflows]\nenabled=true\nunknown=1",
        "[idle_workflows]\nenabled='yes'",
        "[[event_triggers]]\nid='x'\nevent='issues.opened'\ncommand='label'\nunknown=true",
        "[[path_triggers.rules]]\nid='x'\ninclude=['src/**']\nunknown=true",
    ] {
        assert_eq!(
            RepositoryConfig::parse(text, REPO).unwrap_err().code,
            ReasonCode::InvalidDocument,
            "{text}"
        );
    }
}

/// Idle semantic failures stay local while the legacy parsing entry point retains validation.
#[test]
fn idle_semantics_are_isolated_but_legacy_validation_stays_strict() {
    let cfg = parse("[idle_workflows]\nenabled=true\nidle_minutes=0");
    assert_eq!(cfg.idle.unwrap_err().code, ReasonCode::InvalidIdle);
    assert!(cfg.comments.is_ok());
    assert!(
        xero_bot::idle_workflows::config::parse("[idle_workflows]\nenabled=true", REPO).is_err()
    );
    assert!(xero_bot::idle_workflows::config::parse(
        "[command_triggers]\nreview={mode='disabled'}",
        REPO
    )
    .unwrap()
    .is_none());
}

/// Automatic subscriptions must honor disabled policies and the explicit action whitelist.
#[test]
fn event_rules_never_enable_unsupported_or_unsafe_actions() {
    for (command, event, extra, reason) in [
        ("review", "pull_request.opened", "", ReasonCode::Unsupported),
        ("codeql", "pull_request.opened", "", ReasonCode::Unsupported),
        (
            "relabel",
            "issues.opened",
            "add=['triage']",
            ReasonCode::Unsupported,
        ),
        ("r+", "pull_request.opened", "", ReasonCode::InvalidRule),
        ("r=", "pull_request.opened", "", ReasonCode::InvalidRule),
        ("r-", "pull_request.opened", "", ReasonCode::InvalidRule),
        ("claim", "pull_request.opened", "", ReasonCode::InvalidRule),
        ("assign", "pull_request.opened", "", ReasonCode::InvalidRule),
        ("review", "issues.opened", "", ReasonCode::InvalidRule),
        (
            "review",
            "pull_request.synchronize",
            "",
            ReasonCode::InvalidRule,
        ),
        ("label", "issues.opened", "", ReasonCode::InvalidRule),
    ] {
        let cfg = parse(&format!(
            "[[event_triggers]]\nid='one'\nevent='{event}'\ncommand='{command}'\n{extra}"
        ));
        assert_eq!(
            cfg.events.unwrap()[0].value.as_ref().unwrap_err().code,
            reason
        );
        assert!(cfg.comments.is_ok());
    }
    let cfg = parse("[command_triggers]\nrelabel={mode='disabled'}\n[[event_triggers]]\nid='one'\nevent='issues.opened'\ncommand='label'\nadd=['triage']");
    assert_eq!(
        cfg.events.unwrap()[0].value.as_ref().unwrap_err().code,
        ReasonCode::Disabled
    );
}

/// One invalid rule stays isolated; duplicate IDs make the entire owning domain unavailable.
#[test]
fn rule_failures_stay_per_rule_and_duplicate_ids_disable_only_the_domain() {
    let rule = "[[event_triggers]]\nid='one'\nevent='pull_request.opened'\ncommand='review'\n";
    let cfg = parse(&format!(
        "{rule}[[event_triggers]]\nid='two'\nevent='bad'\ncommand='review'"
    ));
    let e = cfg.events.unwrap();
    assert_eq!(e.len(), 2);
    assert_eq!(
        e[0].value.as_ref().unwrap_err().code,
        ReasonCode::Unsupported
    );
    assert_eq!(
        e[1].value.as_ref().unwrap_err().code,
        ReasonCode::InvalidRule
    );
    let cfg = parse(&format!("{rule}{rule}"));
    assert_eq!(cfg.events.unwrap_err().code, ReasonCode::DuplicateRuleId);
    assert!(cfg.comments.is_ok());
    assert!(cfg.paths.is_ok());
    assert!(cfg.idle.is_ok());
}

/// Path actions remain independent of comment modes, with shared event and CC-budget limits.
#[test]
fn paths_have_independent_static_actions_and_shared_domain_limits() {
    let rule = "[[path_triggers.rules]]\nid='rust'\ninclude=['src/**/*.rs']\nexclude=['src/generated/**']\nlabels=['rust']\ncc=['alice']\n";
    let cfg = parse(&format!(
        "[command_triggers]\nlabel={{mode='disabled'}}\ncc={{mode='disabled'}}\n{rule}"
    ));
    assert_eq!(
        cfg.paths.unwrap().rules[0].value.as_ref().unwrap_err().code,
        ReasonCode::Unsupported
    );
    for settings in [
        "max_cc_users_per_pr=11",
        "max_cc_users_per_pr=-1",
        "events=['issues.opened']",
        "events=[]",
    ] {
        let cfg = parse(&format!("[path_triggers]\n{settings}\n{rule}"));
        assert_eq!(cfg.paths.unwrap_err().code, ReasonCode::InvalidPathSettings);
        assert!(cfg.comments.is_ok());
    }
    assert_eq!(
        parse(&format!("[path_triggers]\nmax_cc_users_per_pr=0\n{rule}"))
            .paths
            .unwrap()
            .max_cc_users_per_pr,
        0
    );
    assert_eq!(
        parse(&format!("{rule}{rule}")).paths.unwrap_err().code,
        ReasonCode::DuplicateRuleId
    );
    let cfg = parse(&format!(
        "{rule}[[path_triggers.rules]]\nid='bad'\ninclude=[]\ncc=['org/team']"
    ));
    let paths = cfg.paths.unwrap();
    assert_eq!(
        paths.rules[0].value.as_ref().unwrap_err().code,
        ReasonCode::Unsupported
    );
    assert_eq!(
        paths.rules[1].value.as_ref().unwrap_err().code,
        ReasonCode::InvalidRule
    );
}

/// The pure gate must order disabled, applicability and mention checks before caller permissions.
#[test]
fn gates_preserve_priority_and_never_grant_execution_authority() {
    let c = Comments::default();
    assert!(c.gate(CommandId::Claim, false, false, false).is_ok());
    assert_eq!(
        c.gate(CommandId::Review, false, false, false)
            .unwrap_err()
            .code,
        ReasonCode::RequiresPr
    );
    assert_eq!(
        c.gate(CommandId::Review, true, false, false)
            .unwrap_err()
            .code,
        ReasonCode::SessionRequired
    );
    assert!(c.gate(CommandId::Review, true, false, true).is_ok());
    assert_eq!(
        c.gate(CommandId::Approve, true, false, true)
            .unwrap_err()
            .code,
        ReasonCode::MentionRequired
    );
    assert!(c.gate(CommandId::Approve, true, true, false).is_ok());
}

/// Malformed approval targets and extra arguments must not emit any executable approval.
#[test]
fn r_equals_never_falls_back_to_plain_approval_with_a_bad_target() {
    for text in [
        "@bot r=",
        "@bot r=alice",
        "@bot r= @-bad",
        "@bot r= @alice extra",
        "@bot r= @alice @bob",
        "@bot r= @alice. extra",
        "@bot r= @alice。请审查",
        "@bot r= @alice . ! 多余参数",
        "@bot r= @alice. @bob",
        "@bot r= @alice。@bob",
        "@bot r= @alice. @-bad",
        "@bot r= @alice。 +label",
        "@bot r= please @alice",
    ] {
        assert!(parse_commands("bot", text).commands.is_empty(), "{text}");
    }
}

/// Sentence punctuation must not invalidate a target or consume a later command.
#[test]
fn r_equals_accepts_trailing_punctuation_and_preserves_command_boundaries() {
    use xero_bot::commands::Command;
    for (text, has_ping) in [
        ("@bot r= @alice.", false),
        ("@bot r= @alice。", false),
        ("@bot r= @alice!", false),
        ("@bot r= @alice，！", false),
        ("@bot r= @alice . ! 。", false),
        ("@bot r= @alice.; ping", true),
        ("@bot r= @alice。\n@bot ping", true),
        ("@bot r= @alice! @bot ping", true),
    ] {
        let output = parse_commands("bot", text);
        let mut expected = vec![Command::Approve {
            on_behalf_of: Some("alice".into()),
        }];
        if has_ping {
            expected.push(Command::Ping);
        }
        assert_eq!(
            output
                .commands
                .into_iter()
                .map(|c| c.command)
                .collect::<Vec<_>>(),
            expected,
            "{text}"
        );
        assert!(
            output.diagnostics.is_empty(),
            "{text}: {:?}",
            output.diagnostics
        );
    }
}

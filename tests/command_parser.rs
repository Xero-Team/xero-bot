//! Issue #12 acceptance: syntax, source evidence and policy-before-resolution.
use xero_bot::commands::{parse_commands, resolve_commands, Command, SourceForm};
use xero_bot::config::repository::{CommandId, Comments, RepositoryConfig};

fn candidates(text: &str) -> Vec<Command> {
    parse_commands("bot", text)
        .commands
        .into_iter()
        .map(|c| c.command)
        .collect()
}

#[test]
fn complete_bare_blocks_accept_aliases_whitespace_and_bounded_arguments() {
    for (text, expected) in [
        ("claim", vec![Command::Claim]),
        (
            "  TaKe \r\n\n untake  ",
            vec![Command::Claim, Command::Unclaim],
        ),
        (
            "take; cc @alice",
            vec![
                Command::Claim,
                Command::Cc {
                    users: vec!["alice".into()],
                },
            ],
        ),
        ("\nreview\ncodeql\n", vec![Command::Review, Command::Codeql]),
        (
            "cc @alice,@Bob; assign @carol\nclaim",
            vec![
                Command::Cc {
                    users: vec!["alice".into(), "Bob".into()],
                },
                Command::Assign {
                    user: "carol".into(),
                },
                Command::Claim,
            ],
        ),
        (
            "label +中文标签 -wip; commands",
            vec![
                Command::Label {
                    add: vec!["中文标签".into()],
                    remove: vec!["wip".into()],
                },
                Command::Help,
            ],
        ),
    ] {
        let parsed = parse_commands("bot", text);
        assert!(parsed.diagnostics.is_empty(), "{text}: {parsed:?}");
        assert_eq!(candidates(text), expected, "{text}");
    }
}

#[test]
fn every_registered_alias_produces_the_canonical_id_in_both_forms() {
    for id in CommandId::ALL {
        for alias in id.aliases() {
            let args = match (id, *alias) {
                (_, "r=") | (CommandId::Cc | CommandId::Assign | CommandId::RequestReview, _) => {
                    " @alice"
                }
                (CommandId::Label, _) => " +triage",
                _ => "",
            };
            for prefix in ["", "@bot "] {
                let text = format!("{prefix}{alias}{args}");
                let out = parse_commands("bot", &text);
                assert_eq!(out.commands.len(), 1, "{text}: {out:?}");
                assert_eq!(out.commands[0].id(), id, "{text}");
                assert_eq!(out.commands[0].is_explicit(), !prefix.is_empty(), "{text}");
            }
        }
    }
}

#[test]
fn bare_prose_and_markup_collisions_are_silent_for_the_entire_comment() {
    for text in [
        "claim 是什么意思？",
        "claim what does this mean?",
        "take this later",
        "review 一下",
        "claim\n可以认领吗？",
        "claim\nWhat happens next?",
        "text\nclaim",
        "claim; hello",
        "claim; cc @alice about this",
        "cc @alice about this",
        "cc @alice 关于这个",
        "cc @alice @bad_name",
        "cc @alice,",
        "assign @alice @bob",
        "assign @alice later",
        "; claim",
        "claim;;",
        "claim; ; cc @alice",
        "claim\n; claim",
        "cc@alice",
        "assign@alice",
        "cc @alice@bob",
        "label+bug",
        "claim.",
        "claim。",
        "!claim",
        ", claim",
        "- claim",
        "* claim",
        "1. claim",
        "claim\n> quotation",
        "> quotation\nclaim",
        "claim `ignored`",
        "`ignored` claim",
        "cl`ignored`aim",
        "claim\n```\nexample\n```",
        "```\nexample\n```\nclaim",
        "claim\n\n    example",
        "    claim",
        "\tclaim",
        "~~~\nclaim\n~~~",
        "claim\n`跨行\n代码`",
        "claim 🎉",
        "claim; @other ping",
        "@other claim",
        "@bottle claim",
        "@bot_extra claim",
        "@bot[bot]extra claim",
        "claim @bot ping", // a nullary bare word cannot end at an adjacent mention
    ] {
        let out = parse_commands("bot", text);
        // Explicit entry points can still be recognized independently.
        assert!(
            out.commands.iter().all(|c| c.is_explicit()),
            "bare candidate for {text}: {out:?}"
        );
        assert!(
            out.diagnostics.is_empty(),
            "prose should stay quiet: {text}: {out:?}"
        );
    }
}

#[test]
fn parameters_are_identical_and_never_absorb_the_next_command() {
    for prefix in ["", "@bot "] {
        for separator in ["; ", "\n"] {
            let text = format!("{prefix}cc @alice,@bob{separator}assign @carol{separator}claim");
            assert_eq!(
                candidates(&text),
                vec![
                    Command::Cc {
                        users: vec!["alice".into(), "bob".into()]
                    },
                    Command::Assign {
                        user: "carol".into()
                    },
                    Command::Claim,
                ],
                "{text}"
            );
        }
        for args in [
            "cc",
            "cc about @alice",
            "cc @alice junk @bob",
            "cc@alice",
            "cc @alice@bob",
            "assign@alice",
            "assign",
            "assign @alice extra",
            "assign @alice @bob",
        ] {
            let text = format!("{prefix}{args}");
            assert!(candidates(&text).is_empty(), "{text}");
        }
    }
    for (text, expected) in [
        ("@bot cc; ping", vec![Command::Ping]),
        ("@bot assign\n@alice", vec![]),
        ("@bot cc\n@alice", vec![]),
        (
            "@bot assign; cc @bob",
            vec![Command::Cc {
                users: vec!["bob".into()],
            }],
        ),
        (
            "@bot cc @alice @bot assign @bob",
            vec![
                Command::Cc {
                    users: vec!["alice".into()],
                },
                Command::Assign { user: "bob".into() },
            ],
        ),
    ] {
        assert_eq!(candidates(text), expected, "{text}");
    }
}

#[test]
fn approvals_share_target_grammar_and_never_degrade_after_an_error() {
    for prefix in ["", "@bot "] {
        for verb in ["r=", "r+ as", "r+"] {
            for ending in ["", ".", "。", "，！"] {
                let text = format!("{prefix}{verb} @Alice{ending}");
                assert_eq!(
                    candidates(&text),
                    vec![Command::Approve {
                        on_behalf_of: Some("Alice".into())
                    }],
                    "{text}"
                );
            }
            for target in [
                "@-bad",
                "@bad-",
                "@a--b",
                "@bad_name",
                "@中文",
                "@alice/bob",
                "@alice[bot]",
                "@alice extra",
                "@alice @bob",
                "@alice。请审查",
                "please @alice",
                "@alice +label",
            ] {
                let text = format!("{prefix}{verb} {target}");
                let out = parse_commands("bot", &text);
                assert!(
                    out.commands.is_empty(),
                    "bad approval emitted: {text}: {out:?}"
                );
                assert!(!out.diagnostics.is_empty(), "missing diagnosis: {text}");
            }
        }
        for bad in [
            "r=",
            "r+ as",
            "r=alice",
            "r+ as;",
            "r+ nonsense",
            "r+ as\n@alice",
            "r=\n@alice",
            "r- extra",
        ] {
            let text = format!("{prefix}{bad}");
            let out = parse_commands("bot", &text);
            assert!(out.commands.is_empty(), "{text}: {out:?}");
            assert!(!out.diagnostics.is_empty(), "{text}");
        }
    }
    assert_eq!(
        candidates("r+"),
        vec![Command::Approve { on_behalf_of: None }]
    );
    assert_eq!(candidates("@bot r+ as; ping"), vec![Command::Ping]);
    assert_eq!(
        candidates("@bot r= @alice; assign @bob"),
        vec![
            Command::Approve {
                on_behalf_of: Some("alice".into())
            },
            Command::Assign { user: "bob".into() },
        ]
    );
    assert!(candidates(&format!("r= @{}", "a".repeat(40))).is_empty());
    assert_eq!(candidates(&format!("r= @{}", "a".repeat(39))).len(), 1);
}

#[test]
fn mention_scope_is_exact_case_insensitive_and_ends_at_newline() {
    let text = "@BoT[BoT] claim; cc @alice\nreview; r+\n@other ping";
    let out = parse_commands("bot", text);
    // The @other line invalidates the whole bare block, but not explicit calls.
    assert_eq!(
        out.commands.iter().map(|c| c.id()).collect::<Vec<_>>(),
        vec![CommandId::Claim, CommandId::Cc]
    );
    assert!(out.commands.iter().all(|c| c.mention_span == Some(0..9)));
    let text = "@BoT[BoT] claim; cc @alice\nreview; r+";
    let out = parse_commands("BOT[bot]", text);
    assert_eq!(out.commands.len(), 4);
    assert_eq!(
        out.commands
            .iter()
            .map(|c| c.is_explicit())
            .collect::<Vec<_>>(),
        [true, true, false, false]
    );
    for wrong in [
        "@bottle",
        "@bot-other",
        "@bot_extra",
        "@bot[bot]extra",
        "@bot[other]",
        "@bot[bot][bot]",
    ] {
        assert!(candidates(&format!("{wrong} ping")).is_empty(), "{wrong}");
    }
}

#[test]
fn symbols_and_explicit_commands_survive_prose_without_extracting_bare_words() {
    for (text, expected) in [
        ("说明：r? @alice\nclaim", vec![CommandId::RequestReview]),
        (
            "looks good ?r @alice cc @bob\nreview",
            vec![CommandId::Ready, CommandId::RequestReview, CommandId::Cc],
        ),
        ("说明：@bot review 一下\nclaim", vec![CommandId::Review]),
        ("@bot, ping please\nclaim", vec![CommandId::Ping]),
    ] {
        assert_eq!(
            parse_commands("bot", text)
                .commands
                .iter()
                .map(|c| c.id())
                .collect::<Vec<_>>(),
            expected,
            "{text}"
        );
    }
}

#[test]
fn code_and_quotes_never_produce_symbolic_or_explicit_candidates() {
    for text in [
        "`@bot r+`",
        "> @bot claim\n> r? @alice",
        "```rs\n@bot ping\nr+\n```",
        "    @bot ping\n    r? @alice\n    @bot r+",
        "\t@bot ping\n\t?r",
        "`multi\n@bot r+\n?r @alice`",
        "`` multi `\n@bot r= @alice\n``",
        "~~~\n?r @alice\n~~~",
        "```rust\n```not-a-closing-fence\n@bot r+\n```",
        "```\n@bot ping",
        "    text\n\n    @bot r+",
    ] {
        let out = parse_commands("bot", text);
        assert!(
            out.commands.is_empty() && out.diagnostics.is_empty(),
            "{text}: {out:?}"
        );
    }
}

#[test]
fn utf8_ranges_and_compound_origins_index_the_original_comment() {
    let text = "中文 K 🎉\n@BoT ?r @alice cc @bob; ping\n?r @carol";
    let out = parse_commands("bot", text);
    assert_eq!(out.commands.len(), 6);
    let pieces = ["?r", "@alice", "cc @bob", "ping", "?r", "@carol"];
    for (i, candidate) in out.commands.iter().enumerate() {
        assert_eq!(&text[candidate.span.clone()], pieces[i]);
        assert_eq!(candidate.id(), candidate.command.id());
        assert_eq!(candidate.is_explicit(), i < 4);
        if let Some(span) = &candidate.mention_span {
            assert_eq!(&text[span.clone()], "@BoT");
        }
        if i < 3 {
            assert_eq!(candidate.source, SourceForm::Symbol);
            assert_eq!(
                &text[candidate.compound_span.clone().unwrap()],
                "?r @alice cc @bob"
            );
        }
    }
    assert!(out.commands[3].compound_span.is_none());
    assert_eq!(out.commands[3].source, SourceForm::ExplicitMention);
    let out = parse_commands("bot", "claim");
    assert_eq!(out.commands[0].source, SourceForm::BareWord);
}

#[test]
fn policy_filters_individual_candidates_before_resolution() {
    let policy = RepositoryConfig::parse("[command_triggers]\nblocked={mode='disabled'}", "codeql")
        .unwrap()
        .comments
        .unwrap();
    for (text, expected) in [
        ("@bot ready; blocked", vec![Command::Ready]),
        ("r+\n@bot r+", vec![Command::Approve { on_behalf_of: None }]),
    ] {
        let mut out = parse_commands("bot", text);
        assert_eq!(out.commands.len(), 2, "must retain all sources");
        assert!(out.diagnostics.is_empty(), "must not resolve early");
        let permitted = out
            .commands
            .into_iter()
            .filter(|c| policy.gate(c.id(), true, c.is_explicit(), false).is_ok())
            .collect();
        assert_eq!(
            resolve_commands("bot", permitted, &mut out.diagnostics),
            expected
        );
        assert!(
            out.diagnostics.is_empty(),
            "denied candidates must not cause conflicts/duplicates"
        );
    }
    let policy = Comments::default();
    let out = parse_commands("bot", "?r @alice cc @bob");
    assert!(out
        .commands
        .iter()
        .all(|c| policy.gate(c.id(), true, c.is_explicit(), false).is_ok()));
    let out = parse_commands("bot", "r= @alice");
    assert!(
        policy
            .gate(out.commands[0].id(), true, false, true)
            .is_err(),
        "even a session cannot bypass always_mention"
    );
}

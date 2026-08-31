use super::*;

#[test]
fn first_tab_on_multiple_matches_selects_first_candidate() {
    let mut app = App::new();
    app.input = "/".into();
    app.cursor_pos = app.input.len();

    app.next_completion();

    assert!(app.completion_active);
    assert_eq!(app.completion_selected, 0);
    assert_eq!(app.completion_candidates[0].0, "/plan");
}

#[test]
fn tab_extends_to_shared_prefix_before_cycling() {
    let mut app = App::new();
    app.input = "/rese".into();
    app.cursor_pos = app.input.len();

    app.next_completion();

    assert_eq!(app.input, "/reset");
    assert!(app.completion_active);
    assert_eq!(
        app.completion_candidates
            .iter()
            .map(|(cmd, _)| *cmd)
            .collect::<Vec<_>>(),
        vec!["/reset-spec", "/reset"]
    );
}

#[test]
fn abbreviate_home_replaces_home_prefix() {
    let home = dirs::home_dir().expect("test environment should have a home directory");
    let path = home.join("Repos").join("ferrus");
    assert_eq!(abbreviate_home(&path), "~/Repos/ferrus");
}

#[test]
fn typing_slash_command_updates_context_without_tab() {
    let mut app = App::new();

    app.insert_char('/');
    app.insert_char('s');

    assert!(app.has_command_context());
    assert_eq!(
        app.completion_candidates
            .iter()
            .map(|(cmd, _)| *cmd)
            .collect::<Vec<_>>(),
        vec!["/spec", "/supervisor", "/status", "/stop"]
    );
    assert!(!app.completion_active);
}

#[test]
fn autocomplete_includes_new_hq_commands_and_omits_execute() {
    let commands: Vec<&str> = COMMANDS.iter().map(|(cmd, _)| *cmd).collect();

    assert!(commands.contains(&"/task"));
    assert!(commands.contains(&"/spec"));
    assert!(commands.contains(&"/check"));
    assert!(commands.contains(&"/tasks"));
    assert!(commands.contains(&"/run"));
    assert!(commands.contains(&"/runs"));
    assert!(commands.contains(&"/events"));
    assert!(commands.contains(&"/model"));
    assert!(commands.contains(&"/resume"));
    assert!(commands.contains(&"/reset-spec"));
    assert!(commands.contains(&"/supervisor"));
    assert!(commands.contains(&"/executor"));
    assert!(!commands.contains(&"/execute"));
}

#[test]
fn render_prompt_wraps_multiline_input() {
    let mut app = App::new();
    app.input = "abcd\nef".into();
    app.cursor_pos = app.input.chars().count();

    let prompt = render_prompt(&app, 6);

    assert_eq!(prompt.lines, vec!["abcd", "ef"]);
    assert_eq!(prompt.cursor_row, 1);
    assert_eq!(prompt.cursor_col, 4);
}

#[test]
fn render_prompt_preserves_trailing_newline() {
    let mut app = App::new();
    app.input = "abcd\n".into();
    app.cursor_pos = app.input.chars().count();

    let prompt = render_prompt(&app, 10);

    assert_eq!(prompt.lines, vec!["abcd", ""]);
    assert_eq!(prompt.cursor_row, 1);
    assert_eq!(prompt.cursor_col, 2);
}

#[test]
fn dashboard_prompt_uses_command_completion_context() {
    let mut app = App::new();
    app.input = "/".into();
    app.cursor_pos = app.input.len();
    app.next_completion();

    let lines = prompt_accessory_lines(&app, 80)
        .into_iter()
        .flat_map(|line| line.segments.into_iter().map(|segment| segment.text))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(lines.contains("Commands"));
}

#[test]
fn header_places_version_box_under_logo() {
    let mut app = App::new();
    app.startup = Some(StartupHeader {
        version: "v0.3.0-alpha.1".into(),
        supervisor_type: "claude-code".into(),
        supervisor_version: "2.1.143 (Claude Code)".into(),
        executor_type: "codex".into(),
        executor_version: "codex-cli 0.132.0".into(),
    });

    let rendered = header_lines(&app, 120)
        .into_iter()
        .map(|line| {
            line.line
                .segments
                .into_iter()
                .map(|segment| segment.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert_eq!(rendered[0], "");
    assert!(rendered[1].starts_with("  ███████"));
    assert_eq!(rendered[6], "");
    assert!(rendered[7].starts_with("  ╭"));
    assert!(rendered[8].contains("version:"));
    assert!(rendered[9].contains("supervisor:"));
}

#[test]
fn dashboard_omits_separator_between_tip_and_project_frame() {
    let app = App::new();
    let rendered = dashboard_lines(&app, 120, 40)
        .into_iter()
        .map(|line| {
            line.line
                .segments
                .into_iter()
                .map(|segment| segment.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let tip_idx = rendered
        .iter()
        .position(|line| line.contains("Tip:"))
        .expect("tip line should be rendered");
    assert!(rendered[tip_idx].starts_with(" Tip:"));
    let next_non_empty = rendered[tip_idx + 1..]
        .iter()
        .find(|line| !line.is_empty())
        .expect("project frame should follow tip");

    assert!(next_non_empty.starts_with("  ╭"));
}

#[test]
fn footer_debug_indicator_is_right_aligned() {
    let mut app = App::new();
    app.debug = true;

    let line = footer_line(&app, 60);
    let segments = line.segments;
    let rendered = segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<String>();
    let debug = segments.last().expect("debug segment should be present");

    assert_eq!(debug.text, "debug");
    assert_eq!(debug.color, Color::DarkBlue);
    assert_eq!(display_width(&rendered), 60);
    assert!(rendered.ends_with("debug"));
}

#[test]
fn footer_line_shows_pending_and_ready_milestones_before_done() {
    let mut app = App::new();
    app.status.selected_milestones = vec![
        MilestoneSnapshot {
            marker: "#1.0".into(),
            title: "Pending".into(),
            completed: false,
            readiness: MilestoneReadiness::Pending,
        },
        MilestoneSnapshot {
            marker: "#1.1".into(),
            title: "Ready".into(),
            completed: false,
            readiness: MilestoneReadiness::Ready,
        },
    ];

    let rendered = footer_line(&app, 120)
        .segments
        .into_iter()
        .map(|segment| segment.text)
        .collect::<String>();

    assert!(rendered.contains("0 waiting  •  1 pending  •  1 ready  •  0 done"));
}

#[test]
fn project_milestone_frame_uses_header_inset() {
    let app = App::new();
    let width = 120;
    let rendered = project_and_milestone_lines(&app, 120)
        .into_iter()
        .map(|row| {
            row.line
                .segments
                .into_iter()
                .map(|segment| segment.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert!(rendered.iter().all(|line| line.starts_with("  ")));
    assert!(rendered[0].starts_with("  ╭"));
    assert!(rendered.iter().all(|line| display_width(line) == width - 2));
}

#[test]
fn project_milestone_frame_rows_keep_terminal_width() {
    let mut app = App::new();
    app.status.directory = "~/Repos/ferrus".into();
    app.status.branch = Some("feature/multi-task".into());
    app.status.selected_milestones = vec![
        MilestoneSnapshot {
            marker: "#1.0".into(),
            title: "Define dashboard layout".into(),
            completed: true,
            readiness: MilestoneReadiness::Done,
        },
        MilestoneSnapshot {
            marker: "#1.1".into(),
            title: "Wire runtime activity".into(),
            completed: false,
            readiness: MilestoneReadiness::Ready,
        },
        MilestoneSnapshot {
            marker: "#2.0".into(),
            title: "Wait for previous milestone".into(),
            completed: false,
            readiness: MilestoneReadiness::Pending,
        },
    ];
    let width = 120;

    let rows = project_and_milestone_lines(&app, width);
    let rendered = rows
        .iter()
        .map(|row| {
            row.line
                .segments
                .first()
                .map(|segment| segment.text.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();

    assert!(rows.len() >= 4);
    for text in &rendered {
        assert_eq!(display_width(text), width - 2);
    }
    assert_eq!(rendered[1].find("Project"), rendered[6].find("tasks:"));
    assert!(rendered[1].contains("│ Project"));
    assert!(rendered[1].contains("│ Milestones"));
    let done_col = rendered[2].find("done").unwrap();
    let ready_col = rendered[3].find("ready").unwrap();
    let pending_col = rendered[4].find("pending").unwrap();
    assert_eq!(done_col, ready_col);
    assert_eq!(done_col, pending_col);
    assert_eq!(char_before_last_border(rendered[2]), Some(' '));
    assert_eq!(char_before_last_border(rendered[3]), Some(' '));
    assert_eq!(char_before_last_border(rendered[4]), Some(' '));
}

#[test]
fn task_counts_line_shows_queued_runtime_tasks() {
    let mut app = App::new();
    app.runtime_tasks = vec![TaskRecord {
        id: "t-001".into(),
        path: ".ferrus/tasks/t-001.md".into(),
        spec_path: None,
        milestone_id: None,
        status: TaskStatus::Pending.as_str().into(),
        paused_status: None,
        claimed_by: None,
        lease_until: None,
        last_heartbeat: None,
        check_retries: 0,
        review_cycles: 0,
        failure_reason: None,
    }];

    assert_eq!(
        task_counts_line(&app),
        "tasks:       0 running  0 waiting  1 queued  0 done"
    );
}

#[test]
fn command_output_renders_in_unframed_activity_area() {
    let mut app = App::new();
    app.messages.push(TranscriptLine {
        text: "status output".into(),
        kind: TranscriptKind::Info,
        continuation: false,
    });

    let rendered = dashboard_lines(&app, 120, 40)
        .into_iter()
        .map(|line| {
            line.line
                .segments
                .into_iter()
                .map(|segment| segment.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    let activity = rendered
        .iter()
        .find(|line| line.contains("status output"))
        .expect("command output should be visible");
    assert_eq!(activity, "  status output");
    let activity_idx = rendered
        .iter()
        .position(|line| line.contains("status output"))
        .unwrap();
    assert_eq!(rendered[activity_idx - 1], "");
}

#[test]
fn success_activity_gets_leading_dot() {
    let line = TranscriptLine {
        text: "Task t-001 completed.".into(),
        kind: TranscriptKind::Success,
        continuation: false,
    };

    assert_eq!(activity_text(&line), "• Task t-001 completed.");
}

#[test]
fn log_activity_path_is_clickable_file_link() {
    let line = transcript_activity_line(
        &TranscriptLine {
            text: "  ╰─ Logs: .ferrus/logs/executor.log".into(),
            kind: TranscriptKind::Muted,
            continuation: true,
        },
        120,
    );

    let link = line
        .segments
        .iter()
        .find_map(|segment| segment.link.as_deref())
        .expect("log path should be linked");
    assert!(link.starts_with("file://"));
    assert!(link.ends_with("/.ferrus/logs/executor.log"));
}

#[test]
fn error_panel_keeps_interactive_stderr_detail() {
    let rendered = error_lines(
        "supervisor agent (codex) exited with exit status: 1\n\nstderr:\nError: Invalid MCP configuration",
        120,
    )
    .into_iter()
    .map(|line| {
        line.line
            .segments
            .into_iter()
            .map(|segment| segment.text)
            .collect::<String>()
    })
    .collect::<Vec<_>>();

    assert!(
        rendered
            .iter()
            .any(|line| line.contains("Error: Invalid MCP configuration")),
        "rendered error did not include stderr detail: {rendered:?}"
    );
}

#[test]
fn runtime_activity_shows_tasks_before_recent_runs() {
    let mut app = App::new();
    app.runtime_tasks = vec![
        TaskRecord {
            id: "t-005".into(),
            path: ".ferrus/tasks/t-005.md".into(),
            spec_path: Some("docs/specs/example.md".into()),
            milestone_id: Some("m1.0".into()),
            status: "pending".into(),
            paused_status: None,
            claimed_by: None,
            lease_until: None,
            last_heartbeat: None,
            check_retries: 0,
            review_cycles: 0,
            failure_reason: None,
        },
        TaskRecord {
            id: "t-006".into(),
            path: ".ferrus/tasks/t-006.md".into(),
            spec_path: None,
            milestone_id: None,
            status: "reviewing".into(),
            paused_status: None,
            claimed_by: Some("supervisor:codex:t-006".into()),
            lease_until: None,
            last_heartbeat: None,
            check_retries: 0,
            review_cycles: 0,
            failure_reason: None,
        },
    ];
    app.runtime_runs = vec![RunRecord {
        id: "run-1".into(),
        task_id: "t-006".into(),
        role: "supervisor".into(),
        agent: "supervisor:codex:t-006".into(),
        status: "running".into(),
        started_at: "2026-05-24T10:00:00Z".into(),
        updated_at: "2026-05-24T10:00:01Z".into(),
        pid: Some(123),
        workspace_path: "/tmp/work".into(),
    }];

    let rendered = activity_area_lines(&app, 120, 10)
        .into_iter()
        .map(|line| {
            line.line
                .segments
                .into_iter()
                .map(|segment| segment.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let pending_idx = rendered
        .iter()
        .position(|line| line.contains("t-005") && line.contains("pending"))
        .unwrap();
    let reviewing_idx = rendered
        .iter()
        .position(|line| line.contains("t-006") && line.contains("reviewing"))
        .unwrap();
    let run_idx = rendered
        .iter()
        .position(|line| line.contains("supervisor") && line.contains("running"))
        .unwrap();

    assert!(pending_idx < run_idx);
    assert!(reviewing_idx < run_idx);
    assert!(rendered[pending_idx].contains("m1.0"));
    assert!(rendered[reviewing_idx].contains("supervisor:codex:t-006"));
}

#[test]
fn command_outputs_are_spaced_without_splitting_continuations() {
    let mut app = App::new();
    app.messages.extend([
        TranscriptLine {
            text: "first command".into(),
            kind: TranscriptKind::Info,
            continuation: false,
        },
        TranscriptLine {
            text: "first detail".into(),
            kind: TranscriptKind::Info,
            continuation: true,
        },
        TranscriptLine {
            text: "second command".into(),
            kind: TranscriptKind::Info,
            continuation: false,
        },
    ]);

    let rendered = activity_area_lines(&app, 120, 20)
        .into_iter()
        .map(|line| {
            line.line
                .segments
                .into_iter()
                .map(|segment| segment.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let first_idx = rendered
        .iter()
        .position(|line| line.contains("first command"))
        .unwrap();
    let detail_idx = rendered
        .iter()
        .position(|line| line.contains("first detail"))
        .unwrap();
    let second_idx = rendered
        .iter()
        .position(|line| line.contains("second command"))
        .unwrap();

    assert_eq!(detail_idx, first_idx + 1);
    assert_eq!(rendered[first_idx - 1], "");
    assert_eq!(rendered[second_idx - 1], "");
}

#[test]
fn activity_area_keeps_latest_multiline_message_start_when_tight() {
    let mut app = App::new();
    app.messages.extend(split_transcript(
        "  • Started supervisor:codex:t-006...\n  ╰─ Logs: .ferrus/logs/supervisor.log",
        TranscriptKind::Muted,
    ));

    let rendered = activity_area_lines(&app, 120, 1)
        .into_iter()
        .map(|line| {
            line.line
                .segments
                .into_iter()
                .map(|segment| segment.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert_eq!(rendered.len(), 1);
    assert!(rendered[0].contains("Started supervisor:codex:t-006"));
}

#[test]
fn activity_area_does_not_spend_only_line_on_gap() {
    let mut app = App::new();
    app.messages.extend(split_transcript(
        "Task t-006 completed.",
        TranscriptKind::Success,
    ));

    let rendered = activity_area_lines(&app, 120, 1)
        .into_iter()
        .map(|line| {
            line.line
                .segments
                .into_iter()
                .map(|segment| segment.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert_eq!(rendered, vec!["  • Task t-006 completed."]);
}

fn char_before_last_border(text: &str) -> Option<char> {
    let mut before = None;
    let mut before_last_border = None;
    for ch in text.chars() {
        if ch == '│' {
            before_last_border = before;
        }
        before = Some(ch);
    }
    before_last_border
}

#[test]
fn elapsed_only_status_update_does_not_change_dashboard() {
    let previous = StatusSnapshot {
        task_state: "Executing".into(),
        task_state_detail: "Executing (1s)".into(),
        ..StatusSnapshot::default()
    };
    let next = StatusSnapshot {
        task_state: "Executing".into(),
        task_state_detail: "Executing (2s)".into(),
        ..StatusSnapshot::default()
    };

    assert!(!status_dashboard_changed(&previous, &next));
}

#[test]
fn multiline_submission_does_not_enter_history() {
    let mut app = App::new();
    let original_history_len = app.history.len();
    app.input = "first\nsecond".into();
    app.cursor_pos = app.input.chars().count();
    app.question_task_id = Some("t-002".to_string());
    app.answering_question_task_id = Some("t-002".to_string());
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

    app.submit_input(&cmd_tx);

    assert_eq!(
        cmd_rx.try_recv().unwrap(),
        HqInput {
            text: "first\nsecond".to_string(),
            human_question_task_id: Some("t-002".to_string()),
        }
    );
    assert_eq!(app.history.len(), original_history_len);
}

#[test]
fn human_answer_keeps_the_question_target_from_typing_start() {
    let mut app = App::new();
    app.question_task_id = Some("t-001".to_string());
    app.insert_text("first line\nsecond line");
    app.question_task_id = Some("t-002".to_string());
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

    app.submit_input(&cmd_tx);

    assert_eq!(
        cmd_rx.try_recv().unwrap(),
        HqInput {
            text: "first line\nsecond line".to_string(),
            human_question_task_id: Some("t-001".to_string()),
        }
    );
}

#[test]
fn pasted_text_preserves_multiline_prompt_newline() {
    let mut app = App::new();
    app.input = "first".into();
    app.cursor_pos = app.input.chars().count();

    app.insert_text("\r\nsecond");

    let prompt = dashboard_prompt(&app, 80);
    let rendered = prompt
        .lines
        .iter()
        .map(|line| {
            line.segments
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert_eq!(app.input, "first\nsecond");
    assert_eq!(rendered, vec!["> first", "  second"]);
    assert_eq!(prompt.cursor_row, 1);
}

#[test]
fn up_moves_within_multiline_before_history() {
    let mut app = App::new();
    app.history = vec!["/status".into()];
    app.input = "one\ntwo\nthree".into();
    app.cursor_pos = app.input.chars().count();

    app.move_up_or_history();
    assert_eq!(
        app.cursor_pos,
        "one\n".chars().count() + "two".chars().count()
    );
    assert_eq!(app.input, "one\ntwo\nthree");
    assert_eq!(app.history_idx, None);

    app.move_up_or_history();
    assert_eq!(app.cursor_pos, "one".chars().count());
    assert_eq!(app.input, "one\ntwo\nthree");
    assert_eq!(app.history_idx, None);

    app.move_up_or_history();
    assert_eq!(app.input, "/status");
    assert_eq!(app.history_idx, Some(0));
}

#[test]
fn down_moves_within_multiline_before_history() {
    let mut app = App::new();
    app.input = "one\ntwo\nthree".into();
    app.cursor_pos = 0;

    app.move_down_or_history();
    assert_eq!(app.cursor_pos, "one\n".chars().count());
    assert_eq!(app.input, "one\ntwo\nthree");
    assert_eq!(app.history_idx, None);

    app.move_down_or_history();
    assert_eq!(app.cursor_pos, "one\ntwo\n".chars().count());
    assert_eq!(app.input, "one\ntwo\nthree");
    assert_eq!(app.history_idx, None);

    app.move_down_or_history();
    assert_eq!(app.cursor_pos, "one\ntwo\n".chars().count());
    assert_eq!(app.input, "one\ntwo\nthree");
    assert_eq!(app.history_idx, None);

    app.move_down_or_history();
    assert_eq!(app.cursor_pos, "one\ntwo\n".chars().count());
    assert_eq!(app.input, "one\ntwo\nthree");
    assert_eq!(app.history_idx, None);
}

#[test]
fn up_uses_history_immediately_for_single_line_input() {
    let mut app = App::new();
    app.history = vec!["/status".into()];
    app.input = "draft".into();
    app.cursor_pos = app.input.chars().count();

    app.move_up_or_history();

    assert_eq!(app.input, "/status");
    assert_eq!(app.history_idx, Some(0));
}

#[test]
fn multiline_enter_accepts_shift_and_alt_enter() {
    assert!(is_multiline_enter(KeyModifiers::SHIFT));
    assert!(is_multiline_enter(KeyModifiers::ALT));
    assert!(is_multiline_enter(KeyModifiers::SHIFT | KeyModifiers::ALT));
    assert!(!is_multiline_enter(KeyModifiers::NONE));
    assert!(!is_multiline_enter(KeyModifiers::CONTROL));
    assert!(!is_multiline_enter(
        KeyModifiers::CONTROL | KeyModifiers::SHIFT
    ));
    assert!(!is_multiline_enter(
        KeyModifiers::CONTROL | KeyModifiers::ALT
    ));
    assert!(!is_multiline_enter(KeyModifiers::SUPER | KeyModifiers::ALT));
}

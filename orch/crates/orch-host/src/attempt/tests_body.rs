    use super::*;
    use std::process::Command;
    use std::time::SystemTime;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // ─────────────── B144 REPORT head 解析面：存量兼容回放 + 接线行为 ───────────────

    /// r48-r50 真实 REPORT frontmatter 样本（逐字摘自 coordination/rounds/*/reports/），
    /// 全部必须经 headSha 兼容映射可解析（历史 REPORT 零破坏）。
    #[test]
    fn b144_legacy_reports_r48_r50_all_parse() {
        let samples: [(&str, &str); 9] = [
            // r48 B131
            (
                "taskId: B131\nagent: executor-desktop\nbranch: task/B131\nheadSha: eeb3e2433c31530bc272dc9ae7f51e40416eb8f6\nwroteAt: 2026-07-26T20:11:47Z\n",
                "eeb3e2433c31530bc272dc9ae7f51e40416eb8f6",
            ),
            // r48 B132
            (
                "taskId: B132\nagent: executor-desktop\nbranch: task/B132\nheadSha: 010900b28118c828c274dc14ca0d5088188d086e\nwroteAt: 2026-07-27T03:17:16Z\n",
                "010900b28118c828c274dc14ca0d5088188d086e",
            ),
            // r48 B133
            (
                "taskId: B133\nagent: executor-desktop\nbranch: task/B133\nheadSha: 4474aa5ebfedc807cb59b89b4a714e1d699345db\nwroteAt: 2026-07-27T04:10:47Z\n",
                "4474aa5ebfedc807cb59b89b4a714e1d699345db",
            ),
            // r49 B134
            (
                "taskId: B134\nagent: executor-desktop\nbranch: task/B134\nheadSha: 69bc3c401395f1f0239f879611ff874c5970c0df\nwroteAt: 2026-07-27T05:41:05Z\n",
                "69bc3c401395f1f0239f879611ff874c5970c0df",
            ),
            // r49 B135
            (
                "taskId: B135\nagent: executor-claw\nbranch: task/B135\nheadSha: 8e67e236dbaa53b2bab0ebdaa5ab2d6350975f96\nwroteAt: 2026-07-27T05:53:09Z\n",
                "8e67e236dbaa53b2bab0ebdaa5ab2d6350975f96",
            ),
            // r49 B136（含 attemptId 键）
            (
                "taskId: B136\nattemptId: B136-A0002\nagent: executor-desktop\nbranch: task/B136\nheadSha: b5f19ac18fd909bf82ba310ffef826f0f877762a\nwroteAt: 2026-07-27T07:30:14Z\n",
                "b5f19ac18fd909bf82ba310ffef826f0f877762a",
            ),
            // r49 B137（含 revision 键）
            (
                "taskId: B137\nagent: executor-opencode\nbranch: task/B137\nheadSha: b2c865af93a9941b08e8d0e8b221809ac6e71ea4\nattemptId: B137-A0002\nrevision: 2\nwroteAt: 2026-07-27T19:05:00Z\n",
                "b2c865af93a9941b08e8d0e8b221809ac6e71ea4",
            ),
            // r50 B141
            (
                "taskId: B141\nagent: executor-opencode\nbranch: task/B141\nheadSha: bf55c89e6c87b10afb59ab5fb05eafb951afbdc0\nwroteAt: 2026-07-27T12:30:39Z\n",
                "bf55c89e6c87b10afb59ab5fb05eafb951afbdc0",
            ),
            // r50 B142
            (
                "taskId: B142\nattemptId: B142-A0002\nagent: executor-desktop\nbranch: task/B142\nheadSha: 97faa95b2a308f148a170f80d79279634e2a71a8\nwroteAt: 2026-07-27T12:31:51Z\n",
                "97faa95b2a308f148a170f80d79279634e2a71a8",
            ),
        ];
        for (fm, expect) in samples {
            let heads = report_head_fields(fm).expect("存量 REPORT 必须可解析");
            assert_eq!(heads.implementation_head, expect, "样本解析值漂移: {fm}");
        }
        // 更早的 r41 已用规范字段 implementationHead（短 SHA 原样保留）。
        let old = report_head_fields(
            "taskId: B85\nagent: executor-desktop\nbranch: task/B85\nimplementationHead: eef5482\nwroteAt: 2026-07-25T04:48:24+0800\n",
        )
        .expect("implementationHead 规范字段可解析");
        assert_eq!(old.implementation_head, "eef5482");
    }

    /// 接线入口行为：§0 段前置（r48/B130 真实形态）也能切出 frontmatter；
    /// 分支不可解析 → 跳过对照提示（不阻断）；自报不在提交链上 → 不一致提示。
    #[test]
    fn b144_claim_check_hint_paths() {
        let root = setup_go_repo("b144-claim-check");
        // §0 前置形态 + 分支不存在 → 跳过对照提示，不 Err。
        let report = "## §0 执行环境自报\nMODEL=gpt-5.6-sol\nDEPTH=high\nCAPTURE=Codex runtime model metadata\n\n---\ntaskId: B130\nagent: executor-desktop\nheadSha: aaaa1111\nwroteAt: 2026-07-26T00:00:00Z\n---\n\n正文\n";
        let note = report_head_claim_check(&root, "B130", report.as_bytes())
            .expect("分支不可解析不得 Err")
            .expect("分支不可解析必须有提示行");
        assert!(note.contains("跳过") && note.contains("不阻断"), "{note}");
        // 分支存在且自报 == 分支 HEAD → 一致，无提示。
        let head = git(&root, &["rev-parse", "main"]);
        git(&root, &["branch", "task/B999", &head]);
        let agreeing = format!("---\nimplementationHead: {head}\nheadSha: {head}\n---\n");
        assert!(
            report_head_claim_check(&root, "B999", agreeing.as_bytes())
                .unwrap()
                .is_none(),
            "一致时不得产生提示行"
        );
        // 自报不在分支提交链上（良构但仓库中不存在的 SHA）→ 不一致提示，不 Err。
        let off_branch = format!("---\nheadSha: {}\n---\n", "9".repeat(40));
        let note = report_head_claim_check(&root, "B999", off_branch.as_bytes())
            .expect("不一致不得 Err")
            .expect("不一致必须有提示行");
        assert!(
            note.contains("不在分支") && note.contains("仅提示"),
            "{note}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 接线入口 Err 语义：双缺/矛盾/无 frontmatter 全部融入既有错误路径。
    #[test]
    fn b144_claim_check_parse_failures_are_errors() {
        let root = setup_go_repo("b144-claim-err");
        let conflict = report_head_claim_check(
            &root,
            "B1",
            b"---\nimplementationHead: aaaa1111\nheadSha: bbbb2222\n---\n",
        )
        .unwrap_err();
        assert!(format!("{conflict:#}").contains("矛盾"), "{conflict:#}");
        let missing = report_head_claim_check(&root, "B1", b"---\ntaskId: B1\n---\n").unwrap_err();
        assert!(
            format!("{missing:#}").contains("缺 implementationHead/headSha"),
            "{missing:#}"
        );
        let no_fm = report_head_claim_check(&root, "B1", b"# no frontmatter\n").unwrap_err();
        assert!(format!("{no_fm:#}").contains("B144"), "{no_fm:#}");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// B144 接线测试脚手架：带一条 modern DispatchIssued 的账本 + GO 文件，
    /// control marker 全部压到过去，保证 evidence（当前 mtime）被判 current。
    /// 返回 (root, report_path, 分支 task/B144T 的 HEAD)；REPORT 由调用方写。
    fn b144_claim_site(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let root = setup_go_repo(tag);
        let base = git(&root, &["rev-parse", "main"]);
        git(&root, &["branch", "task/B144T", &base]);
        let go = make_go(&root, "rT", "executor-claw", "GO-B144T-A0001.md");
        // GO mtime 压到过去：marker = max(旧 ULID, GO mtime) 仍在过去。
        std::fs::File::options()
            .write(true)
            .open(&go)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
            .unwrap();
        let ledger_dir = root.join("coordination/rounds/rT");
        std::fs::create_dir_all(&ledger_dir).unwrap();
        let dispatch = format!(
            "{{\"eventId\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"ts\":\"2020-01-01T00:00:00Z\",\"actor\":\"runtime:orch\",\"type\":\"DispatchIssued\",\"taskId\":\"B144T\",\"round\":\"rT\",\"payload\":{{\"agent\":\"executor-claw\",\"attemptId\":\"B144T-A0001\",\"attemptNo\":1,\"baseSha\":\"{base}\",\"goPath\":\"coordination/rounds/rT/dispatch/executor-claw/GO-B144T-A0001.md\"}}}}\n"
        );
        std::fs::write(ledger_dir.join("events.jsonl"), dispatch).unwrap();
        let report_path = ledger_dir.join("reports/B144T-REPORT.md");
        std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
        (root, report_path, base)
    }

    fn b144_report_claim_decision(
        _events: &[EventRecord],
        _ctx: &DispatchContext,
        _marker: Option<std::time::SystemTime>,
        _claimed: Option<&ClaimedEvidence>,
        _epoch: &str,
    ) -> Result<ClaimDecision> {
        Ok(ClaimDecision::Append(vec![crate::ledger::event(
            "ReportObserved",
            "runtime:orch",
            Some("B144T"),
            Some("rT"),
            serde_json::json!({"actionId": "report-observed"}),
        )]))
    }

    /// 接线在 claim 路径内生效：矛盾 REPORT 的 claim 必须 Err（摘除接线本测试即红）。
    #[test]
    fn b144_claim_rejects_report_with_conflicting_heads() {
        let (root, report_path, _head) = b144_claim_site("b144-claim-conflict");
        std::fs::write(
            &report_path,
            b"---\ntaskId: B144T\nimplementationHead: aaaa1111\nheadSha: bbbb2222\nwroteAt: 2020-01-01T00:00:00Z\n---\n",
        )
        .unwrap();
        let observed = observe_evidence(&report_path).unwrap().unwrap();
        let outcome = claim_attempt_action(
            &root,
            "rT",
            "B144T",
            &AttemptObservation {
                attempt_id: Some("B144T-A0001".into()),
                attempt_no: Some(1),
            },
            Some(&observed),
            &b144_report_claim_decision,
        );
        let error = outcome.expect_err("矛盾 REPORT 必须经 claim 错误路径失败");
        assert!(format!("{error:#}").contains("矛盾"), "{error:#}");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 接线不改变既有判定：自报 == 分支 HEAD 的存量形态 REPORT 照常 claim 成功。
    #[test]
    fn b144_claim_accepts_report_with_consistent_heads() {
        let (root, report_path, head) = b144_claim_site("b144-claim-ok");
        std::fs::write(
            &report_path,
            format!("---\ntaskId: B144T\nheadSha: {head}\nwroteAt: 2020-01-01T00:00:00Z\n---\n"),
        )
        .unwrap();
        let observed = observe_evidence(&report_path).unwrap().unwrap();
        let outcome = claim_attempt_action(
            &root,
            "rT",
            "B144T",
            &AttemptObservation {
                attempt_id: Some("B144T-A0001".into()),
                attempt_no: Some(1),
            },
            Some(&observed),
            &b144_report_claim_decision,
        )
        .expect("存量形态 REPORT 必须照常 claim 成功");
        match outcome {
            ClaimOutcome::Appended { count, .. } => assert_eq!(count, 1),
            other => panic!("期望 Appended，实际 {other:?}"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn unique_repo(tag: &str) -> std::path::PathBuf {
        let seq = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "orch-b97-internal-{tag}-{}-{seq}-{nanos}",
            std::process::id()
        ))
    }

    fn init_repo() -> std::path::PathBuf {
        let root = unique_repo("hook");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch@test.invalid",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        );
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        let wt = root.join(".worktrees/B97");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/B97",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        root
    }

    #[test]
    fn modern_dispatch_record_requires_complete_exact_identity_tuple() {
        let derived = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let canonical = "coordination/rounds/r44/dispatch/executor-test/GO-B97-A0001.md";
        let make = |attempt_id: bool, attempt_no: bool, go_path: bool| {
            let mut payload = serde_json::json!({
                "agent": "executor-test",
                "baseSha": "base"
            });
            if attempt_id {
                payload["attemptId"] = serde_json::json!("B97-A0001");
            }
            if attempt_no {
                payload["attemptNo"] = serde_json::json!(1);
            }
            if go_path {
                payload["goPath"] = serde_json::json!(canonical);
            }
            EventRecord {
                event_id: format!("partial-{attempt_id}-{attempt_no}-{go_path}"),
                ts: "2026-07-25T00:00:00Z".into(),
                actor: "runtime:test".into(),
                kind: "DispatchIssued".into(),
                task_id: Some("B97".into()),
                round: Some("r44".into()),
                payload: Some(payload),
                extra: serde_json::Map::new(),
            }
        };
        for mask in 1u8..7 {
            let event = make(mask & 1 != 0, mask & 2 != 0, mask & 4 != 0);
            assert!(
                dispatch_record_for_attempt(&event, &derived).is_err(),
                "partial modern mask {mask:03b} must fail"
            );
        }
        let attempt_id_only = make(true, false, false);
        assert_eq!(
            current_attempt(&[attempt_id_only], "B97").unwrap().unwrap(),
            derived,
            "pure current_attempt inference keeps attemptId-only compatibility"
        );
        let complete = make(true, true, true);
        assert_eq!(
            dispatch_record_for_attempt(&complete, &derived)
                .unwrap()
                .go_path,
            canonical
        );
        let mut wrong_path = complete;
        wrong_path.payload.as_mut().unwrap()["goPath"] =
            serde_json::json!("coordination/rounds/r44/dispatch/executor-test/GO-B97.md");
        assert!(dispatch_record_for_attempt(&wrong_path, &derived).is_err());
    }

    #[test]
    fn snapshot_publishes_no_archive_ref_on_postcheck_mismatch() {
        // Deterministic real mismatch: the hook mutates the worktree's index
        // AFTER commit-tree but BEFORE postchecks. Postcheck must detect the
        // changed index, return Err, AND no archive ref may be published.
        let root = init_repo();
        let wt = root.join(".worktrees/B97");
        fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();

        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let archive_ref = format!("archive/r44-{}-wip", attempt.attempt_id);

        // Hook: stage a new file into the REAL index (simulates a changed site)
        let result = snapshot_worktree_wip_with_hook(
            &root,
            "r44",
            &attempt,
            &mut |phase, _root, worktree| {
                if phase != "between-captures" {
                    return Ok(());
                }
                let out = Command::new("git")
                    .arg("-C")
                    .arg(worktree)
                    .args(["add", "untracked.txt"])
                    .output()?;
                if !out.status.success() {
                    anyhow::bail!(
                        "hook git add failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                Ok(())
            },
        );
        assert!(result.is_err(), "postcheck mismatch must return Err");

        // No archive ref must exist — use raw Command (not git helper which asserts success)
        let ref_check = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "--verify", &format!("refs/{archive_ref}")])
            .output()
            .unwrap();
        assert!(
            !ref_check.status.success(),
            "no archive ref must be published on mismatch, but got: {}",
            String::from_utf8_lossy(&ref_check.stdout)
        );

        // Restore the changed test site
        let _ = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["reset", "-q", "untracked.txt"])
            .output();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_publishes_archive_ref_when_postcheck_passes() {
        let root = init_repo();
        let wt = root.join(".worktrees/B97");
        fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();
        let snapshot = snapshot_worktree_wip(
            &root,
            "r44",
            &AttemptRef {
                task_id: "B97".into(),
                ordinal: 1,
                attempt_id: "B97-A0001".into(),
            },
        )
        .unwrap()
        .unwrap();
        let ref_check = git(
            &root,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/{}", snapshot.archive_ref),
            ],
        );
        assert!(!ref_check.is_empty(), "archive ref should exist");
        fs::remove_dir_all(root).unwrap();
    }

    // ── audit item 2: malformed identity tests ──

    fn event(id: &str, kind: &str, task: &str, payload: serde_json::Value) -> EventRecord {
        EventRecord {
            event_id: id.into(),
            ts: "2026-07-25T00:00:00Z".into(),
            actor: "runtime:test".into(),
            kind: kind.into(),
            task_id: Some(task.into()),
            round: Some("r44".into()),
            payload: Some(payload),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn malformed_attempt_id_non_string_is_err() {
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "x", "baseSha": "b0", "attemptId": 123
            }),
        )];
        assert!(current_attempt(&events, "B97").is_err());
    }

    #[test]
    fn malformed_attempt_id_null_is_err() {
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "x", "baseSha": "b0", "attemptId": serde_json::Value::Null
            }),
        )];
        assert!(current_attempt(&events, "B97").is_err());
    }

    #[test]
    fn malformed_attempt_no_non_integer_is_err() {
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "x", "baseSha": "b0", "attemptId": "B97-A0001", "attemptNo": "not-a-number"
            }),
        )];
        assert!(current_attempt(&events, "B97").is_err());
    }

    #[test]
    fn malformed_attempt_no_wrong_value_is_err() {
        // attemptNo=5 but ordinal=1
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "x", "baseSha": "b0", "attemptId": "B97-A0001", "attemptNo": 5
            }),
        )];
        assert!(current_attempt(&events, "B97").is_err());
    }

    #[test]
    fn malformed_attempt_no_null_is_err() {
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "x", "baseSha": "b0", "attemptId": "B97-A0001", "attemptNo": serde_json::Value::Null
            }),
        )];
        assert!(current_attempt(&events, "B97").is_err());
    }

    #[test]
    fn mixed_explicit_then_legacy_derives_correct_ordinal() {
        // A0001(explicit) → A0002(legacy, no attemptId field at all)
        let events = vec![
            event(
                "d1",
                "DispatchIssued",
                "B97",
                serde_json::json!({
                    "agent": "a", "baseSha": "b0", "attemptId": "B97-A0001", "attemptNo": 1,
                    "goPath": "coordination/rounds/r44/dispatch/a/GO-B97-A0001.md"
                }),
            ),
            event(
                "d2",
                "DispatchIssued",
                "B97",
                serde_json::json!({
                    "agent": "b", "baseSha": "b1"  // no attemptId/attemptNo = legacy
                }),
            ),
        ];
        let cur = current_attempt(&events, "B97").unwrap().unwrap();
        assert_eq!(cur.ordinal, 2);
        assert_eq!(cur.attempt_id, "B97-A0002");
    }

    #[test]
    fn legacy_dispatch_missing_all_identity_fields_is_ok() {
        let events = vec![event(
            "d1",
            "DispatchIssued",
            "B97",
            serde_json::json!({
                "agent": "a", "baseSha": "b0"
            }),
        )];
        let cur = current_attempt(&events, "B97").unwrap().unwrap();
        assert_eq!(cur.attempt_id, "B97-A0001");
    }

    // ── audit item 5: sub-second marker ──

    #[test]
    fn ulid_timestamp_decodes_valid_ulid() {
        // A real ULID generated by the project's own ulid crate
        let u = ulid::Ulid::new();
        let t = ulid_timestamp(&u.to_string());
        assert!(t.is_some(), "valid ULID should decode timestamp");
        // Should be close to now
        let now = SystemTime::now();
        let diff = now
            .duration_since(t.unwrap())
            .unwrap_or(std::time::Duration::ZERO);
        assert!(
            diff.as_secs() < 10,
            "decoded timestamp should be close to now"
        );
    }

    #[test]
    fn ulid_timestamp_rejects_invalid_ulid() {
        // Non-26-char / garbage / valid prefix garbage
        assert!(
            ulid_timestamp("short").is_none(),
            "short string should fail"
        );
        assert!(
            ulid_timestamp("0123456789ABCDEFGHIJ").is_none(),
            "20 chars should fail"
        );
        assert!(
            ulid_timestamp("0123456789ABCDEFGHIJKLMNOPQRSTUVWX!").is_none(),
            "non-crockford char should fail"
        );
        assert!(
            ulid_timestamp("not-a-ulid-at-all-xyz").is_none(),
            "garbage should fail"
        );
    }

    #[test]
    fn ulid_timestamp_fallback_to_rfc3339_on_invalid() {
        // Invalid ULID in event_id, but valid RFC3339 ts — should fallback
        let ev = EventRecord {
            event_id: "bad-ulid".into(),
            ts: "2026-07-25T10:30:00Z".into(),
            actor: "test".into(),
            kind: "DispatchIssued".into(),
            task_id: Some("B97".into()),
            round: Some("r44".into()),
            payload: Some(serde_json::json!({})),
            extra: serde_json::Map::new(),
        };
        let t = control_event_timestamp(&ev);
        assert!(t.is_some(), "should fallback to RFC3339 ts");
    }

    #[test]
    fn ulid_timestamp_prefers_ulid_over_rfc3339() {
        // Valid ULID in event_id, also has RFC3339 ts — should prefer ULID (higher precision)
        let u = ulid::Ulid::new();
        let ev = EventRecord {
            event_id: u.to_string(),
            ts: "2020-01-01T00:00:00Z".into(), // old ts, ULID is now
            actor: "test".into(),
            kind: "DispatchIssued".into(),
            task_id: Some("B97".into()),
            round: Some("r44".into()),
            payload: Some(serde_json::json!({})),
            extra: serde_json::Map::new(),
        };
        let t = control_event_timestamp(&ev).unwrap();
        // ULID timestamp should be close to now, not 2020
        let now = SystemTime::now();
        let diff = now.duration_since(t).unwrap_or(std::time::Duration::ZERO);
        assert!(
            diff.as_secs() < 10,
            "should prefer ULID timestamp, not old RFC3339"
        );
    }

    #[test]
    fn compute_evidence_marker_takes_max() {
        let sig = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        let go_mt = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(200);
        // max(100, 200) = 200
        assert_eq!(compute_evidence_marker(Some(sig), Some(go_mt)), Some(go_mt));
        // max(200, 100) = 200
        assert_eq!(compute_evidence_marker(Some(go_mt), Some(sig)), Some(go_mt));
        // only sig
        assert_eq!(compute_evidence_marker(Some(sig), None), Some(sig));
        // only go_mt
        assert_eq!(compute_evidence_marker(None, Some(go_mt)), Some(go_mt));
        // none
        assert_eq!(compute_evidence_marker(None, None), None);
    }

    #[test]
    fn stale_evidence_between_truncated_ts_and_go_mtime_is_rejected() {
        // 账本 ts 截整秒 = 100s。GO 文件 mtime = 100.5s（晚于截秒，纳秒精度）。
        // 旧 REPORT mtime = 100.3s（晚于截秒 event ts，但早于新 GO 文件 mtime）。
        // marker = max(100s, 100.5s) = 100.5s。
        // evidence 100.3s <= 100.5s ⇒ rejected。
        let sig = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        let go_mt = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(100500);
        let marker = compute_evidence_marker(Some(sig), Some(go_mt));
        let stale_report = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(100300);
        assert!(!evidence_is_current(Some(stale_report), marker));
        // fresh REPORT mtime = 100.7s > 100.5s ⇒ accepted
        let fresh_report = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(100700);
        assert!(evidence_is_current(Some(fresh_report), marker));
    }

    // ── resolve_go_path_strict tests ──

    fn setup_go_repo(tag: &str) -> std::path::PathBuf {
        let root = unique_repo(tag);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::write(root.join("README.md"), "base\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        );
        root
    }

    fn make_go(root: &Path, round: &str, agent: &str, filename: &str) -> std::path::PathBuf {
        let dir = root.join(format!("coordination/rounds/{round}/dispatch/{agent}"));
        std::fs::create_dir_all(&dir).unwrap();
        let go = dir.join(filename);
        std::fs::write(&go, "# GO\n").unwrap();
        go
    }

    #[test]
    fn resolve_go_modern_path_accepted() {
        let root = setup_go_repo("go-modern");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let go = make_go(&root, "r44", "executor-opencode", "GO-B97-A0001.md");
        let go_str = go.to_string_lossy().to_string();
        let abs =
            resolve_go_path_strict(&root, &go_str, "r44", "executor-opencode", &attempt, false)
                .unwrap();
        assert_eq!(abs, go);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_modern_wrong_attempt_rejected() {
        let root = setup_go_repo("go-wrong-aid");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 2,
            attempt_id: "B97-A0002".into(),
        };
        let go = make_go(&root, "r44", "executor-opencode", "GO-B97-A0001.md");
        let go_str = go.to_string_lossy().to_string();
        let result =
            resolve_go_path_strict(&root, &go_str, "r44", "executor-opencode", &attempt, false);
        assert!(result.is_err(), "modern with wrong attemptId should fail");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_legacy_accepted_only_when_legacy_dispatch() {
        let root = setup_go_repo("go-legacy");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let go = make_go(&root, "r44", "executor-opencode", "GO-B97.md");
        let go_str = go.to_string_lossy().to_string();
        // legacy dispatch = true → accept GO-B97.md
        let abs =
            resolve_go_path_strict(&root, &go_str, "r44", "executor-opencode", &attempt, true)
                .unwrap();
        assert_eq!(abs, go);
        // modern dispatch = false → reject GO-B97.md
        let result =
            resolve_go_path_strict(&root, &go_str, "r44", "executor-opencode", &attempt, false);
        assert!(result.is_err(), "modern with legacy filename should fail");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_traversal_rejected() {
        let root = setup_go_repo("go-traversal");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        // relative path with ..
        let go_str = "coordination/rounds/r44/dispatch/executor-opencode/../../GO-B97-A0001.md";
        make_go(&root, "r44", "executor-opencode", "GO-B97-A0001.md");
        let result =
            resolve_go_path_strict(&root, go_str, "r44", "executor-opencode", &attempt, false);
        assert!(result.is_err(), "traversal via ../ should be rejected");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("..") || err.contains("ParentDir"),
            "error should mention traversal: {err}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_curdir_component_rejected_before_normalization() {
        let root = setup_go_repo("go-curdir");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        make_go(&root, "r44", "executor-opencode", "GO-B97-A0001.md");
        let go_str = "coordination/rounds/r44/dispatch/executor-opencode/./GO-B97-A0001.md";
        assert!(
            resolve_go_path_strict(&root, go_str, "r44", "executor-opencode", &attempt, false)
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_nested_rejected() {
        let root = setup_go_repo("go-nested");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        // 7 components (extra nesting)
        let go_str = "coordination/rounds/r44/dispatch/executor-opencode/subdir/GO-B97-A0001.md";
        let result =
            resolve_go_path_strict(&root, go_str, "r44", "executor-opencode", &attempt, false);
        assert!(result.is_err(), "nested path should be rejected");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_missing_file_rejected() {
        let root = setup_go_repo("go-missing");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let go_str = "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md";
        // don't create the file
        let result =
            resolve_go_path_strict(&root, go_str, "r44", "executor-opencode", &attempt, false);
        assert!(result.is_err(), "missing file should be rejected");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_symlink_rejected() {
        let root = setup_go_repo("go-symlink");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        // create a real GO file at a different location
        let real_go_dir = root.join("coordination/rounds/r44/dispatch/executor-opencode/real");
        std::fs::create_dir_all(&real_go_dir).unwrap();
        let real_go = real_go_dir.join("GO-B97-A0001.md");
        std::fs::write(&real_go, "# GO\n").unwrap();
        // create symlink AT the exact expected filename pointing to the real file
        let symlink_go =
            root.join("coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_go, &symlink_go).unwrap();
        #[cfg(not(unix))]
        {
            std::fs::remove_dir_all(root).unwrap();
            return;
        }
        let go_str = symlink_go.to_string_lossy().to_string();
        let result =
            resolve_go_path_strict(&root, &go_str, "r44", "executor-opencode", &attempt, false);
        // Must fail because the expected filename is a symlink, not a regular file
        assert!(
            result.is_err(),
            "symlink at exact expected filename should be rejected"
        );
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("symlink"), "error must mention symlink: {err}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn ack_observation_requires_exact_regular_non_symlink_slot() {
        let root = setup_go_repo("ack-strict");
        let go = make_go(&root, "r44", "executor-opencode", "GO-B97-A0001.md");
        let ack = std::path::PathBuf::from(format!("{}.ack", go.display()));
        assert!(!ack_observed_strict(&go).unwrap());
        std::fs::write(&ack, "ack\n").unwrap();
        assert!(ack_observed_strict(&go).unwrap());
        std::fs::remove_file(&ack).unwrap();
        let target = ack.with_file_name("real-ack");
        std::fs::write(&target, "ack\n").unwrap();
        std::os::unix::fs::symlink(&target, &ack).unwrap();
        assert!(ack_observed_strict(&go).is_err());
        std::fs::remove_file(&ack).unwrap();
        std::os::unix::fs::symlink(ack.with_file_name("missing-ack"), &ack).unwrap();
        assert!(ack_observed_strict(&go).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_go_absolute_outside_root_rejected() {
        let root = setup_go_repo("go-outside");
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        // absolute path outside root
        // O28：固定名临时目录会被并发跑门的另一个进程 remove_dir_all 掉（r45 实测：
        // 收取链与执行者自跑门同时进行 → NotFound）。改用已有的 pid+seq+nanos 助手。
        let outside = unique_repo("go-outside-external");
        std::fs::create_dir_all(&outside).unwrap();
        let go_str = outside.join("GO-B97-A0001.md");
        std::fs::write(&go_str, "# GO\n").unwrap();
        let go_str_abs = go_str.to_string_lossy().to_string();
        let result = resolve_go_path_strict(
            &root,
            &go_str_abs,
            "r44",
            "executor-opencode",
            &attempt,
            false,
        );
        assert!(
            result.is_err(),
            "absolute path outside root should be rejected"
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn m7_go_worktree_instruction_does_not_force_b_when_branch_exists() {
        // M7 回归：已有 branch/worktree 不应要求 -b 创建
        let neither = go_worktree_instruction("B97", "sha", false, false).unwrap();
        assert!(
            neither.contains("-b task/"),
            "neither exists should use -b task/"
        );

        let branch_only = go_worktree_instruction("B97", "sha", true, false).unwrap();
        assert!(
            !branch_only.contains("-b task/"),
            "branch exists should NOT use -b task/: {}",
            branch_only
        );

        let both = go_worktree_instruction("B97", "sha", true, true).unwrap();
        assert!(
            !both.contains("-b task/"),
            "both exist should NOT use -b task/: {}",
            both
        );

        assert!(go_worktree_instruction("B97", "sha", false, true).is_err());
    }

    #[test]
    fn fold_filters_by_action_and_task_identity() {
        // 行为回归：fold 精确筛选 expectation 身份（action_id/task_id），
        // 账本中并存多个 action/task 的交错事件不互相污染。
        fn durable_event(
            kind: &str,
            task: &str,
            action_id: &str,
            owner: &str,
            gen: &str,
        ) -> orch_core::EventRecord {
            orch_core::EventRecord {
                event_id: format!("e-{kind}-{action_id}"),
                ts: "2026-07-25T00:00:00Z".into(),
                actor: "runtime:test".into(),
                kind: kind.into(),
                task_id: Some(task.into()),
                round: Some("r44".into()),
                payload: Some(serde_json::json!({
                    "attemptId": "B97-A0004",
                    "attemptNo": 4,
                    "agent": "executor-opencode",
                    "baseSha": "base",
                    "goPath": "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md",
                    "actionId": action_id,
                    "owner": owner,
                    "leaseGeneration": gen,
                })),
                extra: serde_json::Map::new(),
            }
        }
        // 交错两个 action 的事件
        let events = vec![
            durable_event("DispatchWakeClaimed", "B97", "action-a", "owner-1", "gen-1"),
            durable_event("DispatchWakeClaimed", "B97", "action-b", "owner-2", "gen-2"),
            durable_event(
                "DispatchWakeLaunching",
                "B97",
                "action-a",
                "owner-1",
                "gen-1",
            ),
            durable_event(
                "DispatchWakeReleased",
                "B97",
                "action-b",
                "owner-2",
                "gen-2",
            ),
        ];
        let expectation = DurableActionExpectation {
            round: "r44".into(),
            task_id: "B97".into(),
            attempt_id: "B97-A0004".into(),
            attempt_no: 4,
            agent: "executor-opencode".into(),
            base_sha: "base".into(),
            go_path: "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md".into(),
            action_id: "action-a".into(),
            evidence_sha256: None,
            evidence_len: None,
            control_epoch: None,
            branch_sha: None,
        };
        // fold 只折叠 action-a 事件，action-b 事件不污染
        let phase =
            fold_durable_action(&events, DurableActionKind::DispatchWake, &expectation).unwrap();
        assert_eq!(phase, DurableActionPhase::Launching);
    }

    #[test]
    fn fold_released_without_claimed_predecessor_fails_closed() {
        // 行为回归：Released 无合法前驱 Claimed 时 fail-closed
        let ev = orch_core::EventRecord {
            event_id: "e-rel".into(),
            ts: "2026-07-25T00:00:00Z".into(),
            actor: "runtime:test".into(),
            kind: "DispatchWakeReleased".into(),
            task_id: Some("B97".into()),
            round: Some("r44".into()),
            payload: Some(serde_json::json!({
                "attemptId": "B97-A0004",
                "attemptNo": 4,
                "agent": "executor-opencode",
                "baseSha": "base",
                "goPath": "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md",
                "actionId": "action-4",
                "owner": "owner-1",
                "leaseGeneration": "gen-1",
            })),
            extra: serde_json::Map::new(),
        };
        let expectation = DurableActionExpectation {
            round: "r44".into(),
            task_id: "B97".into(),
            attempt_id: "B97-A0004".into(),
            attempt_no: 4,
            agent: "executor-opencode".into(),
            base_sha: "base".into(),
            go_path: "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md".into(),
            action_id: "action-4".into(),
            evidence_sha256: None,
            evidence_len: None,
            control_epoch: None,
            branch_sha: None,
        };
        assert!(fold_durable_action(&[ev], DurableActionKind::DispatchWake, &expectation).is_err());
    }

    // ── revision 8：fold 严格性 / 多代 lease / ack 对象身份 / legacy logical fold ──

    fn durable_state_action(
        kind: &str,
        owner: &str,
        gen: &str,
        action_id: &str,
    ) -> orch_core::EventRecord {
        orch_core::EventRecord {
            event_id: format!("e-{kind}-{owner}-{gen}-{action_id}"),
            ts: "2026-07-25T00:00:00Z".into(),
            actor: "runtime:test".into(),
            kind: kind.into(),
            task_id: Some("B97".into()),
            round: Some("r44".into()),
            payload: Some(serde_json::json!({
                "attemptId": "B97-A0004",
                "attemptNo": 4,
                "agent": "executor-opencode",
                "baseSha": "base",
                "goPath": "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md",
                "actionId": action_id,
                "owner": owner,
                "leaseGeneration": gen,
            })),
            extra: serde_json::Map::new(),
        }
    }

    fn durable_state(kind: &str, owner: &str, gen: &str) -> orch_core::EventRecord {
        durable_state_action(kind, owner, gen, "action-4")
    }

    fn durable_expectation() -> DurableActionExpectation {
        DurableActionExpectation {
            round: "r44".into(),
            task_id: "B97".into(),
            attempt_id: "B97-A0004".into(),
            attempt_no: 4,
            agent: "executor-opencode".into(),
            base_sha: "base".into(),
            go_path: "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md".into(),
            action_id: "action-4".into(),
            evidence_sha256: None,
            evidence_len: None,
            control_epoch: None,
            branch_sha: None,
        }
    }

    #[test]
    fn fold_supports_multi_generation_reclaim_and_rejects_stale_generation() {
        let expectation = durable_expectation();
        // Claim(g1) → Release(g1) → Claim(g2) → Launching(g2)：合法多代 reclaim。
        let legal = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeReleased", "owner-1", "gen-1"),
            durable_state("DispatchWakeClaimed", "owner-2", "gen-2"),
            durable_state("DispatchWakeLaunching", "owner-2", "gen-2"),
        ];
        assert_eq!(
            fold_durable_action(&legal, DurableActionKind::DispatchWake, &expectation).unwrap(),
            DurableActionPhase::Launching
        );
        // 旧 generation 的 Released 在新 claim 之后 → lineage 违反 fail-closed。
        let mut stale = legal.clone();
        stale.push(durable_state("DispatchWakeReleased", "owner-1", "gen-1"));
        assert!(
            fold_durable_action(&stale, DurableActionKind::DispatchWake, &expectation).is_err()
        );
        // 未 Released 的重复 Claim → Err。
        let double_claim = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
        ];
        assert!(
            fold_durable_action(&double_claim, DurableActionKind::DispatchWake, &expectation)
                .is_err()
        );
        // Released 之后再次 Released（phase 不在 kind 前驱表内）→ Err。
        let double_release = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeReleased", "owner-1", "gen-1"),
            durable_state("DispatchWakeReleased", "owner-1", "gen-1"),
        ];
        assert!(fold_durable_action(
            &double_release,
            DurableActionKind::DispatchWake,
            &expectation
        )
        .is_err());
        // Completed 之后 Released → 非法前驱 fail-closed。
        let release_after_completed = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeLaunching", "owner-1", "gen-1"),
            durable_state("DispatchWakeDelivered", "owner-1", "gen-1"),
            durable_state("DispatchWakeCompleted", "owner-1", "gen-1"),
            durable_state("DispatchWakeReleased", "owner-1", "gen-1"),
        ];
        assert!(fold_durable_action(
            &release_after_completed,
            DurableActionKind::DispatchWake,
            &expectation
        )
        .is_err());
    }

    #[test]
    fn fold_rejects_missing_wrong_type_and_wrong_value_identity_fields() {
        let base_payload = || {
            serde_json::json!({
                "attemptId": "B97-A0004",
                "attemptNo": 4,
                "agent": "executor-opencode",
                "baseSha": "base",
                "goPath": "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0004.md",
                "actionId": "action-4",
                "owner": "owner-1",
                "leaseGeneration": "gen-1",
            })
        };
        let claim_with = |payload: serde_json::Value| {
            vec![orch_core::EventRecord {
                event_id: "e-claim".into(),
                ts: "2026-07-25T00:00:00Z".into(),
                actor: "runtime:test".into(),
                kind: "DispatchWakeClaimed".into(),
                task_id: Some("B97".into()),
                round: Some("r44".into()),
                payload: Some(payload),
                extra: serde_json::Map::new(),
            }]
        };
        for field in [
            "attemptId",
            "attemptNo",
            "agent",
            "baseSha",
            "goPath",
            "actionId",
            "owner",
            "leaseGeneration",
        ] {
            let mut missing = base_payload();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                fold_durable_action(
                    &claim_with(missing),
                    DurableActionKind::DispatchWake,
                    &durable_expectation()
                )
                .is_err(),
                "{field} 缺失必须 Err"
            );
            let mut wrong_type = base_payload();
            wrong_type[field] = serde_json::json!(123);
            assert!(
                fold_durable_action(
                    &claim_with(wrong_type),
                    DurableActionKind::DispatchWake,
                    &durable_expectation()
                )
                .is_err(),
                "{field} 类型错误必须 Err"
            );
        }
        // 值错误（非 owner/generation 的固定字段）→ Err；actionId 值错误视为
        // foreign → scope 内无事件 → Err。
        for field in [
            "attemptId",
            "attemptNo",
            "agent",
            "baseSha",
            "goPath",
            "actionId",
        ] {
            let mut wrong_value = base_payload();
            wrong_value[field] = serde_json::json!("wrong-value");
            assert!(
                fold_durable_action(
                    &claim_with(wrong_value),
                    DurableActionKind::DispatchWake,
                    &durable_expectation()
                )
                .is_err(),
                "{field} 值错误必须 Err"
            );
        }
        // 空串身份字段 → Err。
        for field in [
            "attemptId",
            "agent",
            "baseSha",
            "goPath",
            "owner",
            "leaseGeneration",
        ] {
            let mut empty = base_payload();
            empty[field] = serde_json::json!("");
            assert!(
                fold_durable_action(
                    &claim_with(empty),
                    DurableActionKind::DispatchWake,
                    &durable_expectation()
                )
                .is_err(),
                "{field} 空串必须 Err"
            );
        }
        // top-level round 错误 / 缺失 → Err。
        let mut wrong_round = claim_with(base_payload()).pop().unwrap();
        wrong_round.round = Some("r45".into());
        assert!(fold_durable_action(
            &[wrong_round],
            DurableActionKind::DispatchWake,
            &durable_expectation()
        )
        .is_err());
        let mut missing_round = claim_with(base_payload()).pop().unwrap();
        missing_round.round = None;
        assert!(fold_durable_action(
            &[missing_round],
            DurableActionKind::DispatchWake,
            &durable_expectation()
        )
        .is_err());
        // lineage：非 Claimed 事件的 owner / generation 必须等于锚点。
        let wrong_owner_lineage = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeLaunching", "owner-2", "gen-1"),
        ];
        assert!(fold_durable_action(
            &wrong_owner_lineage,
            DurableActionKind::DispatchWake,
            &durable_expectation()
        )
        .is_err());
        let wrong_gen_lineage = vec![
            durable_state("DispatchWakeClaimed", "owner-1", "gen-1"),
            durable_state("DispatchWakeLaunching", "owner-1", "gen-2"),
        ];
        assert!(fold_durable_action(
            &wrong_gen_lineage,
            DurableActionKind::DispatchWake,
            &durable_expectation()
        )
        .is_err());
    }

    #[test]
    fn scope_has_events_distinguishes_first_call_from_history() {
        let expectation = durable_expectation();
        assert!(!durable_action_scope_has_events(
            &[],
            DurableActionKind::DispatchWake,
            &expectation
        ));
        // foreign action（不同非空 actionId）不算历史。
        let foreign = vec![durable_state_action(
            "DispatchWakeClaimed",
            "owner-9",
            "gen-9",
            "action-9",
        )];
        assert!(!durable_action_scope_has_events(
            &foreign,
            DurableActionKind::DispatchWake,
            &expectation
        ));
        // 本 action 的任意事件 → 有历史。
        let own = vec![durable_state("DispatchWakeClaimed", "owner-1", "gen-1")];
        assert!(durable_action_scope_has_events(
            &own,
            DurableActionKind::DispatchWake,
            &expectation
        ));
        // 不同 kind prefix 不算。
        assert!(!durable_action_scope_has_events(
            &own,
            DurableActionKind::ReportCollect,
            &expectation
        ));

        // Same action but wrong round, or malformed/missing actionId, is not
        // a pristine first call. Production must enter the common fold and
        // fail closed on the immutable identity violation.
        let mut wrong_round = own[0].clone();
        wrong_round.round = Some("r45".into());
        assert!(durable_action_scope_has_events(
            &[wrong_round],
            DurableActionKind::DispatchWake,
            &expectation
        ));
        let mut missing_action = own[0].clone();
        missing_action
            .payload
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("actionId");
        assert!(durable_action_scope_has_events(
            &[missing_action],
            DurableActionKind::DispatchWake,
            &expectation
        ));
        let mut wrong_type_action = own[0].clone();
        wrong_type_action.payload.as_mut().unwrap()["actionId"] = serde_json::json!(7);
        assert!(durable_action_scope_has_events(
            &[wrong_type_action],
            DurableActionKind::DispatchWake,
            &expectation
        ));
    }

    #[test]
    fn ack_same_inode_partial_recovery_completes_when_object_unchanged() {
        // positive-success：partial recovery 捕获的 ack 对象未被替换时，
        // GO mutation 放行并完成归档。
        let root = unique_repo("ack-positive");
        let go_rel =
            "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md".to_string();
        let go_src = root.join(&go_rel);
        let ack_src = std::path::PathBuf::from(format!("{}.ack", go_src.display()));
        let go_dst = root.join(
            "coordination/rounds/r44/dispatch/executor-opencode/superseded/B97-A0001/GO-B97-A0001.md",
        );
        let ack_dst = std::path::PathBuf::from(format!("{}.ack", go_dst.display()));
        fs::create_dir_all(go_src.parent().unwrap()).unwrap();
        fs::create_dir_all(ack_dst.parent().unwrap()).unwrap();
        fs::write(&go_src, b"go-bytes").unwrap();
        fs::write(&ack_src, b"authorized-ack").unwrap();
        fs::hard_link(&ack_src, &ack_dst).unwrap();

        let mut saw_final_seam = false;
        archive_superseded_dispatch_with_hook(
            &root,
            &go_rel,
            "B97-A0001",
            "executor-opencode",
            &mut |phase| {
                if phase == "before-hard-link" {
                    saw_final_seam = true;
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(saw_final_seam, "positive path must cross exact final seam");
        assert!(!go_src.exists(), "GO source 必须已归档");
        assert!(!ack_src.exists(), "ack source 必须已归档");
        assert_eq!(fs::read(&go_dst).unwrap(), b"go-bytes");
        assert_eq!(fs::read(&ack_dst).unwrap(), b"authorized-ack");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ack_authorized_object_removed_before_go_mutation_fails_closed() {
        // replacement-failure 的退化形：授权 ack 在 GO mutation 前被删除 → fail-closed。
        let root = unique_repo("ack-removed");
        let go_rel =
            "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md".to_string();
        let go_src = root.join(&go_rel);
        let ack_src = std::path::PathBuf::from(format!("{}.ack", go_src.display()));
        let go_dst = root.join(
            "coordination/rounds/r44/dispatch/executor-opencode/superseded/B97-A0001/GO-B97-A0001.md",
        );
        let ack_dst = std::path::PathBuf::from(format!("{}.ack", go_dst.display()));
        fs::create_dir_all(go_src.parent().unwrap()).unwrap();
        fs::create_dir_all(ack_dst.parent().unwrap()).unwrap();
        fs::write(&go_src, b"go-bytes").unwrap();
        fs::write(&ack_src, b"authorized-ack").unwrap();
        fs::hard_link(&ack_src, &ack_dst).unwrap();

        let result = archive_superseded_dispatch_with_hook(
            &root,
            &go_rel,
            "B97-A0001",
            "executor-opencode",
            &mut |phase| {
                if phase == "before-move" {
                    fs::remove_file(&ack_dst)?;
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(go_src.exists(), "GO 必须保持 live");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ack_replaced_at_before_hard_link_seam_keeps_go_live() {
        let root = unique_repo("ack-final-seam-replaced");
        let go_rel =
            "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md".to_string();
        let go_src = root.join(&go_rel);
        let ack_src = std::path::PathBuf::from(format!("{}.ack", go_src.display()));
        let go_dst = root.join(
            "coordination/rounds/r44/dispatch/executor-opencode/superseded/B97-A0001/GO-B97-A0001.md",
        );
        let ack_dst = std::path::PathBuf::from(format!("{}.ack", go_dst.display()));
        fs::create_dir_all(go_src.parent().unwrap()).unwrap();
        fs::create_dir_all(ack_dst.parent().unwrap()).unwrap();
        fs::write(&go_src, b"go-live").unwrap();
        fs::write(&ack_src, b"authorized-ack").unwrap();
        fs::hard_link(&ack_src, &ack_dst).unwrap();

        let mut replaced = false;
        let result = archive_superseded_dispatch_with_hook(
            &root,
            &go_rel,
            "B97-A0001",
            "executor-opencode",
            &mut |phase| {
                if phase == "before-hard-link" && !replaced {
                    fs::remove_file(&ack_dst)?;
                    fs::write(&ack_dst, b"replacement-ack")?;
                    replaced = true;
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(replaced);
        assert_eq!(fs::read(&go_src).unwrap(), b"go-live");
        assert!(!go_dst.exists(), "GO destination must not be published");
        fs::remove_dir_all(root).unwrap();
    }

    fn legacy_record_for(task: &str, attempt_id: &str) -> DispatchRecord {
        DispatchRecord {
            attempt: AttemptRef {
                task_id: task.into(),
                ordinal: 1,
                attempt_id: attempt_id.into(),
            },
            agent: "executor-opencode".into(),
            base_sha: Some("legacy-base".into()),
            go_path: format!("coordination/rounds/r44/dispatch/executor-opencode/GO-{task}.md"),
            is_legacy: true,
            previous_attempt_id: None,
            previous_agent: None,
            reassignment: false,
            has_companion_reassignment: false,
            wake_pending: false,
            wake_completed: false,
        }
    }

    #[test]
    fn legacy_resolver_folds_live_and_destination_per_schema_into_one_candidate() {
        // scoped-destination-only → 必须返回 scoped basename（不是 legacy basename）。
        let root = unique_repo("legacy-scoped-dst");
        let scoped_dst = root.join(
            "coordination/rounds/r44/dispatch/executor-opencode/superseded/B97-A0001/GO-B97-A0001.md",
        );
        fs::create_dir_all(scoped_dst.parent().unwrap()).unwrap();
        fs::write(&scoped_dst, b"archived-scoped").unwrap();
        let rel =
            resolve_superseded_go(&root, "r44", &legacy_record_for("B97", "B97-A0001")).unwrap();
        assert_eq!(
            rel,
            "coordination/rounds/r44/dispatch/executor-opencode/GO-B97-A0001.md"
        );
        fs::remove_dir_all(root).unwrap();

        // cross-schema：legacy live + scoped destination 共存 → fail-closed。
        let root = unique_repo("legacy-cross");
        let legacy_live = root.join("coordination/rounds/r44/dispatch/executor-opencode/GO-B97.md");
        fs::create_dir_all(legacy_live.parent().unwrap()).unwrap();
        fs::write(&legacy_live, b"live-legacy").unwrap();
        let scoped_dst = root.join(
            "coordination/rounds/r44/dispatch/executor-opencode/superseded/B97-A0001/GO-B97-A0001.md",
        );
        fs::create_dir_all(scoped_dst.parent().unwrap()).unwrap();
        fs::write(&scoped_dst, b"archived-scoped").unwrap();
        assert!(
            resolve_superseded_go(&root, "r44", &legacy_record_for("B97", "B97-A0001")).is_err()
        );
        fs::remove_dir_all(root).unwrap();

        // zero candidates：live 与 destination 均缺 → fail-closed。
        let root = unique_repo("legacy-zero");
        fs::create_dir_all(&root).unwrap();
        assert!(
            resolve_superseded_go(&root, "r44", &legacy_record_for("B97", "B97-A0001")).is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_final_capture_catches_mutation_before_publish() {
        // final capture 回归：postchecks 之后、CAS publish 之前的同状态内容变更
        // （porcelain/HEAD/index 形状不变）必须被 tree3 捕获，不发布 archive ref。
        let root = init_repo();
        let wt = root.join(".worktrees/B97");
        fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let result = snapshot_worktree_wip_with_hook(
            &root,
            "r44",
            &attempt,
            &mut |phase, _root, worktree| {
                if phase == "after-final-capture" {
                    fs::write(worktree.join("untracked.txt"), "mutated\n")?;
                }
                Ok(())
            },
        );
        assert!(result.is_err(), "final capture 必须捕获发布前变更");
        let ref_check = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "--verify", "refs/archive/r44-B97-A0001-wip"])
            .output()
            .unwrap();
        assert!(!ref_check.status.success(), "变更后不得发布 archive ref");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_ref_publish_fence_catches_mutation_and_leaves_no_ref() {
        let root = init_repo();
        let wt = root.join(".worktrees/B97");
        fs::write(wt.join("untracked.txt"), "before\n").unwrap();
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 6,
            attempt_id: "B97-A0006".into(),
        };
        let result = snapshot_worktree_wip_with_hook(
            &root,
            "r44",
            &attempt,
            &mut |phase, _root, worktree| {
                if phase == "before-ref-publish" {
                    fs::write(worktree.join("untracked.txt"), "mutated-at-publish\n")?;
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        let archive = "refs/archive/r44-B97-A0006-wip";
        let output = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["show-ref", "--verify", "--quiet", archive])
            .status()
            .unwrap();
        assert!(!output.success(), "publish-fence failure must leave no ref");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_refuses_when_cooperative_lock_is_held() {
        let root = init_repo();
        let wt = root.join(".worktrees/B97");
        fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();
        let attempt = AttemptRef {
            task_id: "B97".into(),
            ordinal: 1,
            attempt_id: "B97-A0001".into(),
        };
        let lock = root.join(".git/orch-snapshot-B97.lock");
        fs::write(&lock, b"held").unwrap();
        assert!(
            snapshot_worktree_wip(&root, "r44", &attempt).is_err(),
            "lock 被持有时必须 fail-closed"
        );
        fs::remove_file(&lock).unwrap();
        assert!(snapshot_worktree_wip(&root, "r44", &attempt)
            .unwrap()
            .is_some());
        assert!(!lock.exists(), "成功路径必须释放 lock");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn temp_index_paths_are_unique_per_call() {
        let root = Path::new("/tmp/nonexistent-orch-b97-temp-index");
        let first = gitx::temp_index_path(root, "B97-A0001-tree1");
        let second = gitx::temp_index_path(root, "B97-A0001-tree1");
        assert_ne!(first, second, "同 tag 连续调用必须产生唯一临时路径");
    }

    fn b204_dispatch(task: &str, ordinal: usize, previous_attempt: Option<&str>) -> EventRecord {
        let attempt = format_attempt_id(task, ordinal);
        let mut payload = serde_json::json!({
            "agent": "executor-desktop",
            "attemptId": attempt,
            "attemptNo": ordinal,
            "baseSha": "a".repeat(40),
            "goPath": format!(
                "coordination/rounds/r62/dispatch/executor-desktop/GO-{attempt}.md"
            ),
            "wakePending": false,
        });
        if let Some(previous_attempt) = previous_attempt {
            payload["previousAttemptId"] = serde_json::json!(previous_attempt);
            payload["previousAgent"] = serde_json::json!("executor-desktop");
            payload["reassignment"] = serde_json::json!(false);
        }
        crate::ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some(task),
            Some("r62"),
            payload,
        )
    }

    fn b204_root_pass(task: &str, ordinal: usize) -> EventRecord {
        let attempt = format_attempt_id(task, ordinal);
        crate::ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some(task),
            Some("r62"),
            serde_json::to_value(crate::verify::RootVerdictPayload {
                verdict: "PASS".into(),
                reason: None,
                ir_revision: 1,
                validation_digest: "b".repeat(64),
                attempt_id: attempt,
                attempt_no: ordinal,
                implementer_agent: "executor-desktop".into(),
                head_sha: "c".repeat(40),
                main_head_sha: "d".repeat(40),
                collect_completed_event_id: format!("collect-{ordinal}"),
                bootstrap_pre_signoff_attempt: None,
                reviews: Vec::new(),
                evidence: Vec::new(),
                gates: Vec::new(),
            })
            .unwrap(),
        )
    }

    fn b204_approved_terminal(
        task: &str,
        ordinal: usize,
        verdict_event_id: &str,
        reason: &str,
    ) -> EventRecord {
        crate::ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some(task),
            Some("r62"),
            serde_json::json!({
                "attemptId": format_attempt_id(task, ordinal),
                "attemptNo": ordinal,
                "agent": "executor-desktop",
                "stage": "approved-reattempt",
                "verdictEventId": verdict_event_id,
                "reason": reason,
            }),
        )
    }

    #[test]
    fn approved_reattempt_supports_replay_and_multiple_generations() {
        let task = "B204T";
        let first_dispatch = b204_dispatch(task, 1, None);
        let first_verdict = b204_root_pass(task, 1);
        let mut events = vec![first_dispatch, first_verdict.clone()];
        match approved_reattempt_decision(&events, "r62", task, "retry one").unwrap() {
            ApprovedReattemptDecision::Append {
                previous,
                next_attempt_id,
                ..
            } => {
                assert_eq!(previous.attempt_id, "B204T-A0001");
                assert_eq!(next_attempt_id, "B204T-A0002");
            }
            _ => panic!("first approved generation must append its terminal"),
        }
        events.push(b204_approved_terminal(
            task,
            1,
            &first_verdict.event_id,
            "retry one",
        ));
        match approved_reattempt_decision(&events, "r62", task, "retry one").unwrap() {
            ApprovedReattemptDecision::AlreadyPrepared {
                previous_attempt_id,
                next_attempt_id,
            } => {
                assert_eq!(previous_attempt_id, "B204T-A0001");
                assert_eq!(next_attempt_id, "B204T-A0002");
            }
            _ => panic!("crash before successor dispatch must replay terminal"),
        }
        events.push(b204_dispatch(task, 2, Some("B204T-A0001")));
        match approved_reattempt_decision(&events, "r62", task, "retry one").unwrap() {
            ApprovedReattemptDecision::AlreadyPrepared {
                previous_attempt_id,
                next_attempt_id,
            } => {
                assert_eq!(previous_attempt_id, "B204T-A0001");
                assert_eq!(next_attempt_id, "B204T-A0002");
            }
            _ => panic!("crash after successor dispatch must replay exact lineage"),
        }
        let second_verdict = b204_root_pass(task, 2);
        events.push(second_verdict);
        match approved_reattempt_decision(&events, "r62", task, "retry one").unwrap() {
            ApprovedReattemptDecision::AlreadyPrepared {
                previous_attempt_id,
                next_attempt_id,
            } => {
                assert_eq!(previous_attempt_id, "B204T-A0001");
                assert_eq!(next_attempt_id, "B204T-A0002");
            }
            _ => panic!("delayed replay must not reinterpret A1->A2 as A2->A3"),
        }
        match approved_reattempt_decision(&events, "r62", task, "retry two").unwrap() {
            ApprovedReattemptDecision::Append {
                previous,
                next_attempt_id,
                ..
            } => {
                assert_eq!(previous.attempt_id, "B204T-A0002");
                assert_eq!(next_attempt_id, "B204T-A0003");
            }
            _ => panic!("a later approved attempt may mint the next generation"),
        }
    }

    #[test]
    fn approved_reattempt_rejects_reason_drift_and_forged_durable_completion() {
        let task = "B204U";
        let verdict = b204_root_pass(task, 1);
        let mut events = vec![b204_dispatch(task, 1, None), verdict.clone()];
        events.push(b204_approved_terminal(
            task,
            1,
            &verdict.event_id,
            "original reason",
        ));
        let error = approved_reattempt_decision(&events, "r62", task, "changed reason")
            .err()
            .expect("changed replay reason must fail")
            .to_string();
        assert!(error.contains("conflicts"), "{error}");

        let task = "B204V";
        let mut forged = vec![b204_dispatch(task, 1, None), b204_root_pass(task, 1)];
        forged.push(crate::ledger::event(
            "DispatchWakeCompleted",
            "runtime:orch",
            Some(task),
            Some("r62"),
            serde_json::json!({
                "actionId": "forged-completion",
                "attemptId": "B204V-A0001",
                "attemptNo": 1,
                "agent": "executor-desktop",
                "baseSha": "a".repeat(40),
                "goPath": "coordination/rounds/r62/dispatch/executor-desktop/GO-B204V-A0001.md",
                "owner": "owner",
                "leaseGeneration": "generation",
            }),
        ));
        let error = approved_reattempt_decision(&forged, "r62", task, "retry")
            .err()
            .expect("terminal without durable predecessors must fail")
            .to_string();
        assert!(
            error.contains("Claimed") || error.contains("前驱"),
            "{error}"
        );
    }

    #[test]
    fn approved_reattempt_crash_terminal_requires_exact_explicit_context() {
        let task = "B204W";
        let reason = "operator-approved retry";
        let verdict = b204_root_pass(task, 1);
        let events = vec![
            b204_dispatch(task, 1, None),
            verdict.clone(),
            b204_approved_terminal(task, 1, &verdict.event_id, reason),
        ];

        let plain = plan_dispatch_locked(&events, task, "executor-desktop", &"d".repeat(40), "r62")
            .err()
            .expect("plain dispatch must reject the approved crash terminal")
            .to_string();
        assert!(plain.contains("explicit --new-attempt"), "{plain}");

        {
            let _wrong = ApprovedReattemptExplicitScope::enter(task, "different reason").unwrap();
            let wrong =
                plan_dispatch_locked(&events, task, "executor-desktop", &"d".repeat(40), "r62")
                    .err()
                    .expect("reason drift must reject the approved crash terminal")
                    .to_string();
            assert!(wrong.contains("exact reason"), "{wrong}");
        }

        let _explicit = ApprovedReattemptExplicitScope::enter(task, reason).unwrap();
        let plan = plan_dispatch_locked(&events, task, "executor-desktop", &"d".repeat(40), "r62")
            .unwrap();
        match plan {
            DispatchPlan::New { next, .. } => {
                assert_eq!(next.attempt.attempt_id, "B204W-A0002");
                assert_eq!(next.base_sha, "a".repeat(40));
            }
            DispatchPlan::Existing { .. } => panic!("explicit recovery must mint A0002"),
        }
    }

    #[test]
    fn approved_reattempt_refuses_live_managed_review_and_attach_writers() {
        let task = "B204X";
        let attempt = "B204X-A0001";
        let wake_id = "019fc204-1111-4222-8333-444455556666";
        let mut events = vec![
            crate::ledger::event(
                "ReviewRequested",
                "runtime:orch",
                Some(task),
                Some("r62"),
                serde_json::json!({"attemptId": attempt, "wakeId": wake_id}),
            ),
            crate::ledger::event(
                "WakeIssued",
                "runtime:orch",
                Some(task),
                Some("r62"),
                serde_json::json!({
                    "attemptId": attempt,
                    "wakeId": wake_id,
                    "controlWakeId": wake_id,
                    "agent": "executor-opencode",
                }),
            ),
        ];
        let live = ensure_no_inflight_managed_review(&events, "r62", task, attempt)
            .unwrap_err()
            .to_string();
        assert!(live.contains("expected one terminal"), "{live}");

        events.push(crate::ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some(task),
            Some("r62"),
            serde_json::json!({
                "wakeId": wake_id,
                "agent": "executor-opencode",
                "managedScopeTerminated": true,
            }),
        ));
        ensure_no_inflight_managed_review(&events, "r62", task, attempt).unwrap();

        events.push(crate::ledger::event(
            "ManagedWakeAttachState",
            "runtime:orch",
            Some(task),
            Some("r62"),
            serde_json::json!({
                "attemptId": attempt,
                "actionId": "attach-action",
                "phase": "claimed",
            }),
        ));
        let attach = ensure_no_inflight_managed_review(&events, "r62", task, attempt)
            .unwrap_err()
            .to_string();
        assert!(
            attach.contains("in-flight managed review attach"),
            "{attach}"
        );
    }

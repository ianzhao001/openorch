/// Link-visible marker proving the B285 integration-test support module loaded.
pub fn contract_loaded() {}

#[cfg(test)]
#[path = "../b289_fixture_round_isolation_support/mod.rs"]
mod b289_fixture_round_isolation_support;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use orch_core::read_ledger;
    use orch_host::plan::require_active_round_ir;
    use orch_host::registry::{run_agent_pin_amendment, AGENT_PIN_AMENDED_EVENT_KIND};

    use super::b289_fixture_round_isolation_support as fixture;

    const ROUND: &str = fixture::SYNTHETIC_ROUND;

    fn fixture_root(label: &str) -> PathBuf {
        fixture::synthesize_signed_round(label)
    }

    fn events(root: &Path) -> Vec<orch_core::EventRecord> {
        let ledger =
            read_ledger(&root.join(format!("coordination/rounds/{ROUND}/events.jsonl"))).unwrap();
        assert!(
            ledger.bad_lines.is_empty(),
            "fixture ledger must be canonical"
        );
        ledger.events
    }

    #[test]
    fn production_amendment_preserves_the_signed_plan_and_attribution() {
        let root = fixture_root("b285-production-amendment");
        require_active_round_ir(&root, ROUND, &events(&root)).expect("fixture starts active");
        let registry_path = root.join("coordination/agents.yaml");
        let before = fs::read_to_string(&registry_path).unwrap();
        let signoffs_before = events(&root)
            .iter()
            .filter(|event| event.kind == "PlanSignedOff")
            .count();

        let record = run_agent_pin_amendment(
            &root,
            fixture::AMENDABLE_AGENT,
            None,
            Some("synthetic-model-b285-canary"),
            None,
            "B285 fixture audited amendment",
        )
        .expect("signed open round amendment must commit");
        let rendered = serde_json::to_value(&record).unwrap();
        for key in [
            "agent",
            "before",
            "after",
            "registryDigestBefore",
            "registryDigestAfter",
            "round",
            "irRevision",
            "actor",
            "reason",
        ] {
            assert!(
                rendered.get(key).is_some(),
                "amendment payload missing {key}"
            );
        }

        let after_events = events(&root);
        require_active_round_ir(&root, ROUND, &after_events)
            .expect("durable delta must keep the original signed plan active");
        assert_eq!(
            after_events
                .iter()
                .filter(|event| event.kind == "PlanSignedOff")
                .count(),
            signoffs_before,
            "agent amendment must never mint PlanSignedOff"
        );
        let durable = after_events.last().expect("amendment event");
        assert_eq!(durable.kind, AGENT_PIN_AMENDED_EVENT_KIND);
        assert_eq!(durable.actor, "runtime:orch");
        assert_eq!(durable.round.as_deref(), Some(ROUND));
        assert!(durable.task_id.is_none());

        let after = fs::read_to_string(&registry_path).unwrap();
        assert!(after.contains("# B289 synthetic registry fixture"));
        assert_eq!(before.lines().count(), after.lines().count());

        let hand_edited = after.replace(
            "model: synthetic-model-b285-canary",
            "model: synthetic-model-b285-manual",
        );
        assert_ne!(hand_edited, after);
        fs::write(&registry_path, hand_edited).unwrap();
        assert!(
            require_active_round_ir(&root, ROUND, &after_events).is_err(),
            "the same bytes without a matching event must still drift"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsigned_and_closed_rounds_refuse_without_writing() {
        let unsigned = fixture::synthesize_unsigned_round("b285-unsigned");
        let path = unsigned.join("coordination/agents.yaml");
        let before = fs::read(&path).unwrap();
        assert!(run_agent_pin_amendment(
            &unsigned,
            fixture::AMENDABLE_AGENT,
            None,
            Some("synthetic-model-unsigned"),
            None,
            "must refuse",
        )
        .is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(unsigned).unwrap();

        let closed = fixture::synthesize_closed_round("b285-closed");
        let path = closed.join("coordination/agents.yaml");
        let before = fs::read(&path).unwrap();
        assert!(run_agent_pin_amendment(
            &closed,
            fixture::AMENDABLE_AGENT,
            None,
            Some("synthetic-model-closed"),
            None,
            "must refuse",
        )
        .is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(closed).unwrap();
    }

    #[test]
    fn ledger_failure_rolls_the_registry_back_byte_for_byte() {
        let root = fixture_root("b285-ledger-rollback");
        let registry_path = root.join("coordination/agents.yaml");
        let registry_before = fs::read(&registry_path).unwrap();
        let ledger_path = root.join(format!("coordination/rounds/{ROUND}/events.jsonl"));
        let ledger_before = fs::read(&ledger_path).unwrap();
        let amendment_count_before = events(&root)
            .iter()
            .filter(|event| event.kind == AGENT_PIN_AMENDED_EVENT_KIND)
            .count();
        // Deliberately diverge the WAL from the tracked ledger. Active-plan
        // preflight reads the tracked facts and passes; append must fail under
        // ledger.lock, exercising the file rollback arm.
        let wal_path = root.join(format!("coordination/runtime/ledger-wal/{ROUND}.jsonl"));
        let mut wal = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&wal_path)
            .unwrap();
        wal.write_all(b"{\"diverged\":true}\n").unwrap();
        wal.sync_all().unwrap();
        drop(wal);

        assert!(run_agent_pin_amendment(
            &root,
            fixture::AMENDABLE_AGENT,
            None,
            Some("synthetic-model-rollback"),
            None,
            "force ledger failure",
        )
        .is_err());
        assert_eq!(
            fs::read(&registry_path).unwrap(),
            registry_before,
            "failed durable append must leave registry byte-identical"
        );
        assert_eq!(
            fs::read(&ledger_path).unwrap(),
            ledger_before,
            "failed transaction must leave the tracked ledger byte-identical"
        );
        assert_eq!(
            events(&root)
                .iter()
                .filter(|event| event.kind == AGENT_PIN_AMENDED_EVENT_KIND)
                .count(),
            amendment_count_before,
            "failed transaction must not append AgentPinAmended"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

//! Original recovery lifecycle, unchanged assertions in shared fixture support.
mod manual_lifecycle_support;

#[test]
fn actorless_open_plan_local_collect_verdict_and_seal_is_complete() {
    manual_lifecycle_support::exercise(true, |_, _| {});
}

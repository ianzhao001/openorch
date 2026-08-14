#[test]
fn await_wait_loop_has_no_outer_shared_lease() {
    let source = include_str!("../src/tierf.rs");
    let start = source.find("pub fn run_await_with_hook(").unwrap();
    let end = source[start..]
        .find("fn run_await_with_hook_inner(")
        .map(|offset| start + offset)
        .unwrap();
    let wrapper = &source[start..end];
    assert!(!wrapper.contains("with_protocol_effect"));
    assert!(source[end..].contains("with_active_tierf_effect("));
    let wait = source.find("fs_rx.recv_timeout(tick)").unwrap();
    let nearest_effect = source[..wait].rfind("with_active_tierf_effect(").unwrap();
    assert!(source[nearest_effect..wait].contains(")?;"));
}

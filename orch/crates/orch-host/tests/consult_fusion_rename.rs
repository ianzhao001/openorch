//! B223 seeded-red contract: panel → fusion 全量改名，**不留别名**。
//!
//! Expected red: compile. `orch_host::fusion` 在 B223 之前不存在。
//!
//! 本卡的要点不是「新名字能用」，而是「旧名字**没了**」——用户裁定不留兼容层，
//! 因为兼容层会让 panel/fusion 双词表长期并存，正是这次改名要消灭的东西。
//!
//! Negative mutations that must turn the named case red:
//! M1. 给成员列表字段加 `#[serde(alias = "panel")]` 或任何其它向后兼容路径
//!     -> `legacy_panel_key_is_rejected` 转绿失败（它要求旧键**被拒**）。
//! M2. 磁盘产物目录仍叫 `panel/` -> `skeleton_uses_fusion_dir` 红。
//! M3. 从新模块 re-export 旧名（`run_panel`/`PanelMember`/`panel_dir`）
//!     -> `no_panel_symbols_remain_in_public_surface` 红。
//! M4. 只改模块名而不改 `ConsultPreset` 的成员字段名 -> `fusion_key_is_accepted` 红。

use std::fs;

use orch_host::consult::{create_consultation_skeleton, parse_consult_presets};
use orch_host::fusion::{run_fusion, FusionMember};

fn presets_yaml(member_key: &str) -> String {
    format!(
        "apiVersion: orch/v1alpha1\n\
         kind: ConsultPresets\n\
         metadata:\n\
         \x20 name: seed\n\
         \x20 updatedAt: \"2026-08-03\"\n\
         presets:\n\
         \x20 - name: seed-preset\n\
         \x20   {member_key}: [consult-codex]\n"
    )
}

#[test]
fn fusion_key_is_accepted() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let parsed = parse_consult_presets(root, &presets_yaml("fusion"))
        .expect("`fusion:` 必须是成员列表的正规键名");
    let preset = parsed
        .presets
        .iter()
        .find(|p| p.name == "seed-preset")
        .expect("seed-preset 必须解析出来");
    assert_eq!(preset.fusion, vec!["consult-codex".to_string()]);
    let members: Vec<FusionMember> = preset.members();
    assert_eq!(members.len(), 1);
}

#[test]
fn legacy_panel_key_is_rejected() {
    // 不留别名：旧键必须响亮失败，而不是被静默接受。
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let err = parse_consult_presets(root, &presets_yaml("panel"))
        .expect_err("`panel:` 旧键必须被拒绝——本卡不留兼容别名");
    let text = format!("{err:#}");
    assert!(
        text.contains("panel") || text.contains("unknown field"),
        "拒绝理由必须点名未知键，实际: {text}"
    );
}

#[test]
fn skeleton_uses_fusion_dir() {
    let root = orch_host::util::test_scratch_dir("b223-skeleton");
    let skeleton = create_consultation_skeleton(&root).expect("创建 consultation 骨架失败");
    assert!(
        skeleton.fusion_dir.ends_with("fusion"),
        "产物目录必须叫 fusion/，实际: {}",
        skeleton.fusion_dir.display()
    );
    assert!(skeleton.fusion_dir.is_dir());
    assert!(
        !skeleton.dir.join("panel").exists(),
        "不得同时留一个 panel/ 目录"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn no_panel_symbols_remain_in_public_surface() {
    // 模块文件本身必须已改名，且旧名不得以 re-export 形式续命。
    let host_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    assert!(
        host_src.join("fusion.rs").is_file(),
        "src/fusion.rs 必须存在"
    );
    assert!(
        !host_src.join("panel.rs").exists(),
        "src/panel.rs 必须已改名，不得保留"
    );
    let lib = fs::read_to_string(host_src.join("lib.rs")).expect("读 lib.rs 失败");
    assert!(lib.contains("pub mod fusion;"), "lib.rs 必须声明 fusion 模块");
    assert!(
        !lib.contains("pub mod panel;"),
        "lib.rs 不得同时保留 panel 模块声明"
    );
    let fusion_src = fs::read_to_string(host_src.join("fusion.rs")).expect("读 fusion.rs 失败");
    for banned in ["fn run_panel", "struct PanelMember", "PanelMember,"] {
        assert!(
            !fusion_src.contains(banned),
            "fusion.rs 不得保留旧名 `{banned}`（不留别名）"
        );
    }
    // 冒烟：新入口名可寻址（编译期即可证明，运行期只作占位断言）。
    let _ = run_fusion as *const ();
}

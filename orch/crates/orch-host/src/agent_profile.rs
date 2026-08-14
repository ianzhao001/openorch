//! Agent profile and candidate-chain validation seam (r43/B93).
//!
//! The planner pre-places this module so the executor can implement it without
//! touching the shared `lib.rs` hot spot.
//!
//! 契约(见 coordination/rounds/r43/tasks/B93.md):
//! 1. profile id / quotaDomain trim 后非空,maxConcurrent > 0,profile id 全局唯一;
//! 2. candidate chain 非空,候选 id 已知且不重复;
//! 3. 每个候选覆盖全部 requiredCapabilities(字符串精确匹配);
//! 4. 候选 qualityClass 不低于 route 要求(高等级可服务低等级);
//! 5. 合法结果保持声明的候选顺序,禁止隐式重排;
//! 6. 错误集合稳定、可诊断、含相关 agent/字段,不 panic。

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// An agent's declared writable sandbox range.
///
/// Registry declarations are deliberately not proof of reachability.  The
/// review wake path turns a declaration into a usable fact only after its
/// provider has read the provisioned site's sentinel and returned the exact
/// nonce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxScope {
    /// No declaration (or an empty declaration): assume only the repository.
    RepoRootOnly,
    /// A non-empty registry declaration that has not been probed yet.
    DeclaredUnverified(Vec<String>),
}

impl SandboxScope {
    /// Declarations never self-verify.  This method is intentionally false for
    /// every value produced from `agents.yaml`; only the live wake probe may
    /// establish reachability for a concrete path.
    pub fn is_verified(&self) -> bool {
        false
    }

    pub fn declared_roots(&self) -> &[String] {
        match self {
            SandboxScope::RepoRootOnly => &[],
            SandboxScope::DeclaredUnverified(roots) => roots,
        }
    }
}

/// Apply the conservative default for the optional
/// `sandbox.writableRoots` registry field.
pub fn sandbox_writable_roots(roots: Option<&[String]>) -> SandboxScope {
    match roots {
        Some(roots) if !roots.is_empty() => SandboxScope::DeclaredUnverified(roots.to_vec()),
        _ => SandboxScope::RepoRootOnly,
    }
}

#[derive(serde::Deserialize)]
struct SandboxRegistry {
    agents: HashMap<String, SandboxRegistryAgent>,
}

#[derive(serde::Deserialize)]
struct SandboxRegistryAgent {
    #[serde(default)]
    sandbox: Option<SandboxDeclaration>,
}

#[derive(serde::Deserialize)]
struct SandboxDeclaration {
    #[serde(rename = "writableRoots", default)]
    writable_roots: Vec<String>,
}

/// Read one agent's optional sandbox declaration from `agents.yaml`.
///
/// Non-empty roots must be absolute and free of control characters so the
/// declaration remains diagnostic data rather than an argument-injection
/// surface.  The returned scope is still unverified.
pub fn load_sandbox_scope(root: &Path, agent: &str) -> Result<SandboxScope, String> {
    let path = root.join("coordination/agents.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("读取 AgentRegistry 失败 {}: {error}", path.display()))?;
    let registry: SandboxRegistry = serde_yaml::from_str(&text)
        .map_err(|error| format!("解析 AgentRegistry 失败 {}: {error}", path.display()))?;
    let entry = registry
        .agents
        .get(agent)
        .ok_or_else(|| format!("AgentRegistry 未注册 agent: {agent}"))?;
    let roots = entry
        .sandbox
        .as_ref()
        .map(|sandbox| sandbox.writable_roots.as_slice());
    if let Some(roots) = roots {
        for declared in roots {
            if declared.trim().is_empty()
                || declared.bytes().any(|byte| byte.is_ascii_control())
                || !Path::new(declared).is_absolute()
            {
                return Err(format!(
                    "agent {agent} sandbox.writableRoots 含非法绝对路径: {declared:?}"
                ));
            }
        }
    }
    Ok(sandbox_writable_roots(roots))
}

/// 质量等级。声明顺序即能力偏序:Critical >= Standard >= Light。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QualityClass {
    Light,
    Standard,
    Critical,
}

/// 执行体档案:身份、能力、配额域、并发上限、质量等级。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfile {
    pub id: String,
    pub capabilities: Vec<String>,
    pub quota_domain: String,
    pub max_concurrent: usize,
    pub quality_class: QualityClass,
}

/// 路由需求:最低质量等级 + 必需能力集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRequirement {
    pub quality_class: QualityClass,
    pub required_capabilities: Vec<String>,
}

/// 校验 profile 集合:非空字段、正并发、id 全局唯一。
/// 返回聚合错误(每条含相关 profile id / 字段),全合法时 `Ok(())`。
pub fn validate_profiles(profiles: &[AgentProfile]) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    for profile in profiles {
        let trimmed_id = profile.id.trim();
        if trimmed_id.is_empty() {
            errors.push(format!(
                "profile id 为空或全空白(quotaDomain={})",
                profile.quota_domain.trim()
            ));
        } else if !seen_ids.insert(trimmed_id.to_string()) {
            errors.push(format!("profile id 重复: {trimmed_id}"));
        }
        if profile.quota_domain.trim().is_empty() {
            errors.push(format!("profile {} quotaDomain 为空或全空白", profile.id));
        }
        if profile.max_concurrent == 0 {
            errors.push(format!("profile {} maxConcurrent 为 0", profile.id));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// 校验候选链:非空、候选已知且不重复、能力全覆盖、质量达标;
/// 合法时按声明顺序原样返回候选 id,禁止隐式重排。
pub fn validate_candidate_chain(
    chain: &[String],
    profiles: &[AgentProfile],
    requirement: &RouteRequirement,
) -> Result<Vec<String>, Vec<String>> {
    let mut errors = Vec::new();

    if chain.is_empty() {
        errors.push("candidate chain 为空".to_string());
    }

    let mut required_seen: HashSet<&str> = HashSet::new();
    for capability in &requirement.required_capabilities {
        if !required_seen.insert(capability.as_str()) {
            errors.push(format!("route requirement capability 重复: {capability}"));
        }
    }

    let by_id: HashMap<&str, &AgentProfile> = profiles
        .iter()
        .map(|profile| (profile.id.trim(), profile))
        .collect();

    let mut chain_seen: HashSet<String> = HashSet::new();
    for candidate in chain {
        let candidate_id = candidate.trim();
        if !chain_seen.insert(candidate_id.to_string()) {
            errors.push(format!("candidate 重复: {candidate_id}"));
            continue;
        }
        let Some(profile) = by_id.get(candidate_id) else {
            errors.push(format!("candidate 未知: {candidate_id}"));
            continue;
        };
        for capability in &requirement.required_capabilities {
            if !profile.capabilities.iter().any(|owned| owned == capability) {
                errors.push(format!(
                    "candidate {} 缺少 capability: {capability}",
                    profile.id
                ));
            }
        }
        if profile.quality_class < requirement.quality_class {
            errors.push(format!(
                "candidate {} qualityClass {:?} 低于 route 要求 {:?}",
                profile.id, profile.quality_class, requirement.quality_class
            ));
        }
    }

    if errors.is_empty() {
        Ok(chain.to_vec())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    //! B93 边界补测(自 integration 种子迁入;种子落位文件保持 byte-identical)。
    use super::*;
    use std::fs;

    fn profile(
        id: &str,
        quota: &str,
        capacity: usize,
        capabilities: &[&str],
        quality_class: QualityClass,
    ) -> AgentProfile {
        AgentProfile {
            id: id.into(),
            capabilities: capabilities
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            quota_domain: quota.into(),
            max_concurrent: capacity,
            quality_class,
        }
    }

    fn requirement(quality_class: QualityClass, capabilities: &[&str]) -> RouteRequirement {
        RouteRequirement {
            quality_class,
            required_capabilities: capabilities
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
        }
    }

    #[test]
    fn blank_profile_id_is_rejected() {
        let profiles = vec![profile("  ", "zhipu", 1, &["rust"], QualityClass::Standard)];
        let errors = validate_profiles(&profiles).unwrap_err();
        assert!(errors.iter().any(|error| error.contains("id")));
    }

    #[test]
    fn empty_candidate_chain_is_rejected() {
        let profiles = vec![profile(
            "codex",
            "openai",
            1,
            &["rust"],
            QualityClass::Critical,
        )];
        let req = requirement(QualityClass::Standard, &["rust"]);
        assert!(validate_candidate_chain(&[], &profiles, &req).is_err());
    }

    #[test]
    fn duplicate_required_capability_is_rejected() {
        let profiles = vec![profile(
            "codex",
            "openai",
            1,
            &["rust"],
            QualityClass::Critical,
        )];
        let req = requirement(QualityClass::Standard, &["rust", "rust"]);
        assert!(validate_candidate_chain(&["codex".into()], &profiles, &req).is_err());
    }

    #[test]
    fn sandbox_registry_defaults_narrow_and_keeps_declarations_unverified() {
        let root = crate::util::test_scratch_dir("agent-profile-sandbox");
        fs::create_dir_all(root.join("coordination")).unwrap();
        fs::write(
            root.join("coordination/agents.yaml"),
            "agents:\n  narrow: {injectable: true}\n  empty:\n    sandbox: {writableRoots: []}\n  wide:\n    sandbox:\n      writableRoots: [/tmp, /private/tmp]\n",
        )
        .unwrap();

        assert_eq!(
            load_sandbox_scope(&root, "narrow").unwrap(),
            SandboxScope::RepoRootOnly
        );
        assert_eq!(
            load_sandbox_scope(&root, "empty").unwrap(),
            SandboxScope::RepoRootOnly
        );
        let wide = load_sandbox_scope(&root, "wide").unwrap();
        assert_eq!(
            wide.declared_roots(),
            &["/tmp".to_string(), "/private/tmp".to_string()]
        );
        assert!(!wide.is_verified());
        fs::remove_dir_all(root).unwrap();
    }
}

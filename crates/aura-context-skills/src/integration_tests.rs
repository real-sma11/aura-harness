#![allow(clippy::needless_pass_by_value)]

use aura_core::AgentId;
use crate::error::SkillError;
use crate::install::{SkillInstallStore, SkillInstallStoreApi, SkillInstallation};
use crate::loader::{SkillLoader, SkillLoaderConfig};
use crate::manager::SkillManager;
use crate::types::SkillSource;
use chrono::Utc;
use rocksdb::{ColumnFamilyDescriptor, DBWithThreadMode, MultiThreaded, Options};
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_db(dir: &Path) -> Arc<DBWithThreadMode<MultiThreaded>> {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.create_missing_column_families(true);

    let cfs = vec![
        ColumnFamilyDescriptor::new("record", Options::default()),
        ColumnFamilyDescriptor::new("agent_meta", Options::default()),
        ColumnFamilyDescriptor::new("inbox", Options::default()),
        ColumnFamilyDescriptor::new("memory_facts", Options::default()),
        ColumnFamilyDescriptor::new("memory_events", Options::default()),
        ColumnFamilyDescriptor::new("memory_procedures", Options::default()),
        ColumnFamilyDescriptor::new("memory_event_index", Options::default()),
        ColumnFamilyDescriptor::new("agent_skills", Options::default()),
    ];

    Arc::new(DBWithThreadMode::<MultiThreaded>::open_cf_descriptors(&opts, dir, cfs).unwrap())
}

fn make_skill_dir(base: &Path, name: &str, desc: &str) {
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {desc}\n---\nBody for {name}."),
    )
    .unwrap();
}

fn make_skill_dir_ext(base: &Path, name: &str, extra_frontmatter: &str) {
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} desc\n{extra_frontmatter}---\nBody for {name}."),
    )
    .unwrap();
}

fn workspace_loader(workspace: &Path) -> SkillLoader {
    SkillLoader::new(SkillLoaderConfig {
        workspace_root: Some(workspace.to_path_buf()),
        ..SkillLoaderConfig::default()
    })
}

fn test_agent_id(label: &str) -> AgentId {
    let mut bytes = [0u8; 32];
    let src = label.as_bytes();
    bytes[..src.len().min(32)].copy_from_slice(&src[..src.len().min(32)]);
    AgentId::new(bytes)
}

// ===========================================================================
// 1. SkillManager end-to-end
// ===========================================================================

#[test]
fn manager_new_loads_skills_from_workspace() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join("skills");
    make_skill_dir(&skills, "deploy", "Deploy the app");
    make_skill_dir(&skills, "test-runner", "Run tests");

    let mgr = SkillManager::new(workspace_loader(tmp.path()));
    let all = mgr.list_all();
    assert_eq!(all.len(), 2);

    let names: Vec<&str> = all.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"deploy"));
    assert!(names.contains(&"test-runner"));
}

#[test]
fn manager_inject_skills_adds_xml_to_prompt() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join("skills");
    make_skill_dir(&skills, "deploy", "Deploy the app");

    let mgr = SkillManager::new(workspace_loader(tmp.path()));

    let mut prompt = "You are an assistant.".to_string();
    mgr.inject_skills(&mut prompt);

    assert!(prompt.starts_with("You are an assistant."));
    assert!(prompt.contains("<available_skills>"));
    assert!(prompt.contains("name=\"deploy\""));
    assert!(prompt.contains("</available_skills>"));
}

#[test]
fn manager_inject_skills_empty_when_no_skills() {
    let tmp = TempDir::new().unwrap();
    // No skills directory at all
    let mgr = SkillManager::new(workspace_loader(tmp.path()));

    let mut prompt = "System prompt.".to_string();
    mgr.inject_skills(&mut prompt);
    assert_eq!(prompt, "System prompt.");
}

#[test]
fn manager_activate_returns_rendered_content() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join("skills");
    let dir = skills.join("greeter");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: greeter\ndescription: Greet someone\n---\nHello $ARGUMENTS! Welcome $0.",
    )
    .unwrap();

    let mgr = SkillManager::new(workspace_loader(tmp.path()));
    let act = mgr.activate("greeter", "world").unwrap();

    assert_eq!(act.skill_name, "greeter");
    assert_eq!(act.rendered_content, "Hello world! Welcome world.");
    assert!(!act.fork_context);
    assert!(act.allowed_tools.is_empty());
}

#[test]
fn manager_get_returns_error_for_nonexistent_skill() {
    let tmp = TempDir::new().unwrap();
    let mgr = SkillManager::new(workspace_loader(tmp.path()));

    let err = mgr.get("no-such-skill").unwrap_err();
    assert!(err.is_not_found());
}

#[test]
fn manager_list_all_and_list_user_invocable() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join("skills");
    make_skill_dir_ext(&skills, "alpha", "user-invocable: true\n");
    make_skill_dir_ext(&skills, "beta", "");
    make_skill_dir_ext(&skills, "gamma", "user-invocable: true\n");

    let mgr = SkillManager::new(workspace_loader(tmp.path()));

    assert_eq!(mgr.list_all().len(), 3);

    let user_invocable = mgr.list_user_invocable();
    assert_eq!(user_invocable.len(), 2);
    let names: Vec<&str> = user_invocable.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"gamma"));
}

#[test]
fn manager_reload_picks_up_new_skills() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join("skills");
    make_skill_dir(&skills, "first", "first skill");

    let mut mgr = SkillManager::new(workspace_loader(tmp.path()));
    assert_eq!(mgr.list_all().len(), 1);

    make_skill_dir(&skills, "second", "second skill");
    mgr.reload();
    assert_eq!(mgr.list_all().len(), 2);
}

#[test]
fn manager_activate_with_indexed_arguments() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join("skills");
    let dir = skills.join("deployer");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: deployer\ndescription: Deploy\n---\nDeploy $ARGUMENTS[0] to $ARGUMENTS[1].",
    )
    .unwrap();

    let mgr = SkillManager::new(workspace_loader(tmp.path()));
    let act = mgr.activate("deployer", "myapp production").unwrap();
    assert_eq!(act.rendered_content, "Deploy myapp to production.");
}

// ===========================================================================
// 2. SkillInstallStore CRUD
// ===========================================================================

#[test]
fn install_store_install_writes_to_db() {
    let tmp = TempDir::new().unwrap();
    let db = test_db(tmp.path());
    let store = SkillInstallStore::new(db);

    let agent1 = test_agent_id("agent-1");
    let inst = SkillInstallation {
        agent_id: agent1,
        skill_name: "deploy".to_string(),
        source_url: Some("https://example.com/deploy".to_string()),
        installed_at: Utc::now(),
        version: Some("1.0.0".to_string()),
    };

    store.install(&inst).unwrap();
    assert!(store.is_installed(agent1, "deploy").unwrap());
}

#[test]
fn install_store_list_for_agent_returns_correct_skills() {
    let tmp = TempDir::new().unwrap();
    let db = test_db(tmp.path());
    let store = SkillInstallStore::new(db);

    let agent1 = test_agent_id("agent-1");
    let agent2 = test_agent_id("agent-2");

    for name in &["alpha", "beta", "gamma"] {
        store
            .install(&SkillInstallation {
                agent_id: agent1,
                skill_name: (*name).to_string(),
                source_url: None,
                installed_at: Utc::now(),
                version: None,
            })
            .unwrap();
    }

    store
        .install(&SkillInstallation {
            agent_id: agent2,
            skill_name: "other".to_string(),
            source_url: None,
            installed_at: Utc::now(),
            version: None,
        })
        .unwrap();

    let agent1_skills = store.list_for_agent(agent1).unwrap();
    assert_eq!(agent1_skills.len(), 3);
    let names: Vec<&str> = agent1_skills.iter().map(|s| s.skill_name.as_str()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
    assert!(names.contains(&"gamma"));
    assert!(!names.contains(&"other"));
}

#[test]
fn install_store_is_installed_returns_true_false() {
    let tmp = TempDir::new().unwrap();
    let db = test_db(tmp.path());
    let store = SkillInstallStore::new(db);

    let agent1 = test_agent_id("agent-1");
    let agent2 = test_agent_id("agent-2");

    assert!(!store.is_installed(agent1, "deploy").unwrap());

    store
        .install(&SkillInstallation {
            agent_id: agent1,
            skill_name: "deploy".to_string(),
            source_url: None,
            installed_at: Utc::now(),
            version: None,
        })
        .unwrap();

    assert!(store.is_installed(agent1, "deploy").unwrap());
    assert!(!store.is_installed(agent1, "other-skill").unwrap());
    assert!(!store.is_installed(agent2, "deploy").unwrap());
}

#[test]
fn install_store_uninstall_removes_installation() {
    let tmp = TempDir::new().unwrap();
    let db = test_db(tmp.path());
    let store = SkillInstallStore::new(db);

    let agent1 = test_agent_id("agent-1");

    store
        .install(&SkillInstallation {
            agent_id: agent1,
            skill_name: "deploy".to_string(),
            source_url: None,
            installed_at: Utc::now(),
            version: None,
        })
        .unwrap();

    assert!(store.is_installed(agent1, "deploy").unwrap());
    store.uninstall(agent1, "deploy").unwrap();
    assert!(!store.is_installed(agent1, "deploy").unwrap());
    assert!(store.list_for_agent(agent1).unwrap().is_empty());
}

// ===========================================================================
// 3. SkillManager with install store
// ===========================================================================

#[test]
fn manager_install_for_agent() {
    let tmp_ws = TempDir::new().unwrap();
    let tmp_db = TempDir::new().unwrap();
    let db = test_db(tmp_db.path());
    let store = Arc::new(SkillInstallStore::new(db));

    let loader = workspace_loader(tmp_ws.path());
    let mgr = SkillManager::with_install_store(loader, store);

    let agent1 = test_agent_id("agent-1");
    let inst = mgr
        .install_for_agent(agent1, "deploy", Some("https://example.com".to_string()), vec![], vec![])
        .unwrap();

    assert_eq!(inst.agent_id, agent1);
    assert_eq!(inst.skill_name, "deploy");
    assert_eq!(inst.source_url.as_deref(), Some("https://example.com"));
}

#[test]
fn manager_list_agent_skills() {
    let tmp_ws = TempDir::new().unwrap();
    let tmp_db = TempDir::new().unwrap();
    let db = test_db(tmp_db.path());
    let store = Arc::new(SkillInstallStore::new(db));

    let loader = workspace_loader(tmp_ws.path());
    let mgr = SkillManager::with_install_store(loader, store);

    let agent1 = test_agent_id("agent-1");
    mgr.install_for_agent(agent1, "deploy", None, vec![], vec![]).unwrap();
    mgr.install_for_agent(agent1, "test-runner", None, vec![], vec![])
        .unwrap();

    let skills = mgr.list_agent_skills(agent1).unwrap();
    assert_eq!(skills.len(), 2);
}

#[test]
fn manager_uninstall_from_agent() {
    let tmp_ws = TempDir::new().unwrap();
    let tmp_db = TempDir::new().unwrap();
    let db = test_db(tmp_db.path());
    let store = Arc::new(SkillInstallStore::new(db));

    let loader = workspace_loader(tmp_ws.path());
    let mgr = SkillManager::with_install_store(loader, store);

    let agent1 = test_agent_id("agent-1");
    mgr.install_for_agent(agent1, "deploy", None, vec![], vec![]).unwrap();
    assert_eq!(mgr.list_agent_skills(agent1).unwrap().len(), 1);

    mgr.uninstall_from_agent(agent1, "deploy").unwrap();
    assert!(mgr.list_agent_skills(agent1).unwrap().is_empty());
}

#[test]
fn manager_without_install_store_returns_error() {
    let tmp = TempDir::new().unwrap();
    let mgr = SkillManager::new(workspace_loader(tmp.path()));

    let agent1 = test_agent_id("agent-1");

    let err = mgr.install_for_agent(agent1, "x", None, vec![], vec![]).unwrap_err();
    assert!(matches!(err, SkillError::Activation(_)));

    let err = mgr.list_agent_skills(agent1).unwrap_err();
    assert!(matches!(err, SkillError::Activation(_)));

    let err = mgr.uninstall_from_agent(agent1, "x").unwrap_err();
    assert!(matches!(err, SkillError::Activation(_)));
}

// ===========================================================================
// 4. SkillError
// ===========================================================================

#[test]
fn skill_error_is_not_found_returns_true_for_not_found() {
    let err = SkillError::NotFound("test".to_string());
    assert!(err.is_not_found());
}

#[test]
fn skill_error_is_not_found_returns_false_for_other_variants() {
    let variants: Vec<SkillError> = vec![
        SkillError::Parse("parse error".to_string()),
        SkillError::InvalidName("bad name".to_string()),
        SkillError::Activation("activation error".to_string()),
        SkillError::CommandExecution("cmd error".to_string()),
        SkillError::Store("store error".to_string()),
    ];
    for err in variants {
        assert!(
            !err.is_not_found(),
            "is_not_found() should be false for {err}"
        );
    }
}

// ===========================================================================
// 5. SkillInstallation serde round-trip
// ===========================================================================

#[test]
fn skill_installation_serde_round_trip() {
    let original = SkillInstallation {
        agent_id: test_agent_id("agent-42"),
        skill_name: "my-skill".to_string(),
        source_url: Some("https://example.com/skill".to_string()),
        installed_at: Utc::now(),
        version: Some("2.1.0".to_string()),
    };

    let json = serde_json::to_string(&original).unwrap();
    let deserialized: SkillInstallation = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.agent_id, original.agent_id);
    assert_eq!(deserialized.skill_name, original.skill_name);
    assert_eq!(deserialized.source_url, original.source_url);
    assert_eq!(deserialized.installed_at, original.installed_at);
    assert_eq!(deserialized.version, original.version);
}

#[test]
fn skill_installation_serde_round_trip_minimal() {
    let agent_a = test_agent_id("a");
    let original = SkillInstallation {
        agent_id: agent_a,
        skill_name: "b".to_string(),
        source_url: None,
        installed_at: Utc::now(),
        version: None,
    };

    let json = serde_json::to_string(&original).unwrap();
    let deserialized: SkillInstallation = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.agent_id, agent_a);
    assert_eq!(deserialized.skill_name, "b");
    assert!(deserialized.source_url.is_none());
    assert!(deserialized.version.is_none());
}

// ===========================================================================
// 6. SkillSource precedence
// ===========================================================================

#[test]
fn skill_source_precedence_ordering() {
    assert!(SkillSource::Workspace.precedence() > SkillSource::AgentPersonal.precedence());
    assert!(SkillSource::AgentPersonal.precedence() > SkillSource::Personal.precedence());
    assert!(SkillSource::Personal.precedence() > SkillSource::Extra(std::path::PathBuf::from("/x")).precedence());
    assert!(SkillSource::Extra(std::path::PathBuf::from("/x")).precedence() > SkillSource::Bundled.precedence());

    assert_eq!(SkillSource::Workspace.precedence(), 5);
    assert_eq!(SkillSource::AgentPersonal.precedence(), 4);
    assert_eq!(SkillSource::Personal.precedence(), 3);
    assert_eq!(SkillSource::Extra(std::path::PathBuf::from("/any")).precedence(), 2);
    assert_eq!(SkillSource::Bundled.precedence(), 1);
}

// ===========================================================================
// 7. Multiple agents with different skills installed
// ===========================================================================

#[test]
fn multiple_agents_different_skills() {
    let tmp = TempDir::new().unwrap();
    let db = test_db(tmp.path());
    let store = SkillInstallStore::new(db);

    let agent_a = test_agent_id("agent-a");
    let agent_b = test_agent_id("agent-b");
    let agent_c = test_agent_id("agent-c");

    let agents = [(agent_a, vec!["deploy", "lint"]), (agent_b, vec!["test-runner"]), (agent_c, vec!["deploy", "test-runner", "lint"])];

    for (agent, skills) in &agents {
        for skill in skills {
            store
                .install(&SkillInstallation {
                    agent_id: *agent,
                    skill_name: skill.to_string(),
                    source_url: None,
                    installed_at: Utc::now(),
                    version: None,
                })
                .unwrap();
        }
    }

    let a_skills = store.list_for_agent(agent_a).unwrap();
    assert_eq!(a_skills.len(), 2);

    let b_skills = store.list_for_agent(agent_b).unwrap();
    assert_eq!(b_skills.len(), 1);
    assert_eq!(b_skills[0].skill_name, "test-runner");

    let c_skills = store.list_for_agent(agent_c).unwrap();
    assert_eq!(c_skills.len(), 3);

    assert!(store.is_installed(agent_a, "deploy").unwrap());
    assert!(!store.is_installed(agent_b, "deploy").unwrap());
    assert!(store.is_installed(agent_c, "deploy").unwrap());
}

// ===========================================================================
// 8. inject_agent_skills filters to only installed skills
// ===========================================================================

#[test]
fn inject_agent_skills_only_includes_installed() {
    let tmp_ws = TempDir::new().unwrap();
    let tmp_db = TempDir::new().unwrap();
    let skills = tmp_ws.path().join("skills");
    make_skill_dir(&skills, "deploy", "Deploy the app");
    make_skill_dir(&skills, "test-runner", "Run tests");
    make_skill_dir(&skills, "lint", "Run linter");

    let db = test_db(tmp_db.path());
    let store = Arc::new(SkillInstallStore::new(db));
    let loader = workspace_loader(tmp_ws.path());
    let mgr = SkillManager::with_install_store(loader, store);

    let agent_id = test_agent_id("inject-test");
    let agent_hex = agent_id.to_hex();
    mgr.install_for_agent(agent_id, "deploy", None, vec![], vec![]).unwrap();
    mgr.install_for_agent(agent_id, "lint", None, vec![], vec![]).unwrap();

    let mut prompt = "You are an assistant.".to_string();
    let injected = mgr.inject_agent_skills(&agent_hex, &mut prompt);

    assert_eq!(injected.len(), 2);
    let names: Vec<&str> = injected.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"deploy"));
    assert!(names.contains(&"lint"));
    assert!(!names.contains(&"test-runner"));

    assert!(prompt.contains("<agent_skills>"));
    assert!(prompt.contains("name=\"deploy\""));
    assert!(prompt.contains("name=\"lint\""));
    assert!(!prompt.contains("name=\"test-runner\""));
    assert!(prompt.contains("Body for deploy."), "skill body should be injected");
    assert!(prompt.contains("Body for lint."), "skill body should be injected");
}

#[test]
fn inject_agent_skills_empty_when_none_installed() {
    let tmp_ws = TempDir::new().unwrap();
    let tmp_db = TempDir::new().unwrap();
    let skills = tmp_ws.path().join("skills");
    make_skill_dir(&skills, "deploy", "Deploy the app");

    let db = test_db(tmp_db.path());
    let store = Arc::new(SkillInstallStore::new(db));
    let loader = workspace_loader(tmp_ws.path());
    let mgr = SkillManager::with_install_store(loader, store);

    let agent_id = test_agent_id("no-skills");
    let agent_hex = agent_id.to_hex();
    let mut prompt = "System prompt.".to_string();
    let injected = mgr.inject_agent_skills(&agent_hex, &mut prompt);

    assert!(injected.is_empty());
    assert_eq!(prompt, "System prompt.");
}

#[test]
fn agent_skill_meta_returns_only_installed_and_model_invocable() {
    let tmp_ws = TempDir::new().unwrap();
    let tmp_db = TempDir::new().unwrap();
    let skills = tmp_ws.path().join("skills");
    make_skill_dir(&skills, "deploy", "Deploy the app");
    make_skill_dir_ext(&skills, "hidden", "disable-model-invocation: true\n");

    let db = test_db(tmp_db.path());
    let store = Arc::new(SkillInstallStore::new(db));
    let loader = workspace_loader(tmp_ws.path());
    let mgr = SkillManager::with_install_store(loader, store);

    let agent_id = test_agent_id("meta-test");
    let agent_hex = agent_id.to_hex();
    mgr.install_for_agent(agent_id, "deploy", None, vec![], vec![]).unwrap();
    mgr.install_for_agent(agent_id, "hidden", None, vec![], vec![]).unwrap();

    let meta = mgr.agent_skill_meta(&agent_hex);
    assert_eq!(meta.len(), 1);
    assert_eq!(meta[0].name, "deploy");
}

// ===========================================================================
// 9. Activation edge cases (through manager)
// ===========================================================================

#[test]
fn activate_skill_dir_substitution() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join("skills");
    let dir = skills.join("reader");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: reader\ndescription: Read files\n---\nRead ${SKILL_DIR}/data.json",
    )
    .unwrap();

    let mgr = SkillManager::new(workspace_loader(tmp.path()));
    let act = mgr.activate("reader", "").unwrap();
    let rendered = act.rendered_content.replace('\\', "/");
    assert!(
        rendered.contains("data.json"),
        "expected SKILL_DIR substitution, got: {rendered}"
    );
}

#[test]
fn activate_nonexistent_skill_returns_not_found() {
    let tmp = TempDir::new().unwrap();
    let mgr = SkillManager::new(workspace_loader(tmp.path()));
    let result = mgr.activate("nonexistent", "args");
    assert!(result.is_err());
    assert!(result.unwrap_err().is_not_found());
}

#[test]
fn activate_with_fork_context_and_allowed_tools() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join("skills");
    let dir = skills.join("restricted");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: restricted\ndescription: Restricted skill\ncontext: fork\nallowed-tools:\n  - read\n  - write\nagent: sub-agent\n---\nRestricted body.",
    )
    .unwrap();

    let mgr = SkillManager::new(workspace_loader(tmp.path()));
    let act = mgr.activate("restricted", "").unwrap();

    assert!(act.fork_context);
    assert_eq!(act.allowed_tools, vec!["read", "write"]);
    assert_eq!(act.agent_type.as_deref(), Some("sub-agent"));
}

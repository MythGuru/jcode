use crate::peer_groups::PeerGroups;

fn member(alias: &str, working_dir: &std::path::Path) -> serde_json::Value {
    serde_json::json!({
        "alias": alias,
        "working_dir": working_dir,
    })
}

fn write_groups(home: &std::path::Path, groups: serde_json::Value) {
    std::fs::create_dir_all(home).expect("create jcode home");
    std::fs::write(
        home.join("peer-groups.json"),
        serde_json::to_vec_pretty(&groups).expect("serialize peer groups"),
    )
    .expect("write peer groups");
}

#[test]
fn missing_peer_groups_file_loads_empty_allowlist() {
    let home = tempfile::TempDir::new().expect("temp home");
    let groups = PeerGroups::load_from_jcode_home(home.path()).expect("missing file is valid");
    assert!(groups.groups().is_empty());
}

#[test]
fn malformed_peer_groups_file_uses_safe_error_prefix() {
    let home = tempfile::TempDir::new().expect("temp home");
    std::fs::write(home.path().join("peer-groups.json"), b"not-json")
        .expect("write malformed file");

    let error = PeerGroups::load_from_jcode_home(home.path()).expect_err("malformed file fails");
    assert!(
        error
            .to_string()
            .starts_with("Peer groups configuration is invalid:")
    );
}

#[test]
fn duplicate_group_names_are_rejected() {
    let home = tempfile::TempDir::new().expect("temp home");
    let first = tempfile::TempDir::new().expect("first project");
    let second = tempfile::TempDir::new().expect("second project");
    let third = tempfile::TempDir::new().expect("third project");
    let fourth = tempfile::TempDir::new().expect("fourth project");
    write_groups(
        home.path(),
        serde_json::json!({
            "version": 1,
            "groups": [
                {"name": "healthview", "members": [member("Atlas", first.path()), member("Eve", second.path())]},
                {"name": "healthview", "members": [member("One", third.path()), member("Two", fourth.path())]},
            ]
        }),
    );

    let error = PeerGroups::load_from_jcode_home(home.path()).expect_err("duplicate group fails");
    assert!(error.to_string().contains("duplicate group name"));
}

#[test]
fn aliases_are_unique_case_insensitively_within_group() {
    let home = tempfile::TempDir::new().expect("temp home");
    let first = tempfile::TempDir::new().expect("first project");
    let second = tempfile::TempDir::new().expect("second project");
    write_groups(
        home.path(),
        serde_json::json!({
            "version": 1,
            "groups": [{
                "name": "healthview",
                "members": [member("Atlas", first.path()), member("atlas", second.path())]
            }]
        }),
    );

    let error = PeerGroups::load_from_jcode_home(home.path()).expect_err("duplicate alias fails");
    assert!(error.to_string().contains("duplicate alias"));
}

#[test]
fn canonical_directories_are_unique_across_groups() {
    let home = tempfile::TempDir::new().expect("temp home");
    let shared = tempfile::TempDir::new().expect("shared project");
    let second = tempfile::TempDir::new().expect("second project");
    let third = tempfile::TempDir::new().expect("third project");
    write_groups(
        home.path(),
        serde_json::json!({
            "version": 1,
            "groups": [
                {"name": "one", "members": [member("Atlas", shared.path()), member("Eve", second.path())]},
                {"name": "two", "members": [member("Other", shared.path()), member("Fourth", third.path())]},
            ]
        }),
    );

    let error = PeerGroups::load_from_jcode_home(home.path()).expect_err("duplicate path fails");
    assert!(
        error
            .to_string()
            .contains("working directory is configured more than once")
    );
}

#[test]
fn loaded_peer_groups_are_a_snapshot() {
    let home = tempfile::TempDir::new().expect("temp home");
    let first = tempfile::TempDir::new().expect("first project");
    let second = tempfile::TempDir::new().expect("second project");
    write_groups(
        home.path(),
        serde_json::json!({
            "version": 1,
            "groups": [{"name": "healthview", "members": [member("Atlas", first.path()), member("Eve", second.path())]}]
        }),
    );

    let snapshot = PeerGroups::load_from_jcode_home(home.path()).expect("load initial snapshot");
    std::fs::remove_file(home.path().join("peer-groups.json")).expect("remove source file");

    assert_eq!(snapshot.groups().len(), 1);
    assert_eq!(snapshot.groups()[0].name, "healthview");
}

use wgtui::load_packages_from_json;

#[test]
fn test_package_parsing() {
    let json_content = r#"[
        {
            "name": "Google Chrome",
            "package_name": ["Google.Chrome"],
            "package_manager": "Winget"
        },
        {
            "name": "Custom Script",
            "package_name": ["echo", "hello"],
            "package_manager": "Custom"
        }
    ]"#;

    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("test_packages.json");
    std::fs::write(&path, json_content).expect("failed to write temp file");

    let packages = load_packages_from_json(&path.to_string_lossy()).unwrap();
    assert_eq!(packages.len(), 2);

    assert_eq!(packages[0].name, "Google Chrome");
    assert_eq!(packages[0].package_name, vec!["Google.Chrome"]);
    assert_eq!(packages[0].package_manager, "Winget");
    assert_eq!(
        packages[0].get_cmd(),
        vec![
            "winget",
            "install",
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
            "--scope",
            "machine",
            "Google.Chrome"
        ]
    );

    assert_eq!(packages[1].name, "Custom Script");
    assert_eq!(packages[1].package_name, vec!["echo", "hello"]);
    assert_eq!(packages[1].package_manager, "Custom");
    assert_eq!(packages[1].get_cmd(), vec!["echo", "hello"]);
}
use super::*;

#[test]
fn mv3_manifest_carries_version_and_pins_key() {
    let orig = serde_json::json!({
        "manifest_version": 2,
        "version": "0.9.7",
        "description": "Bridge AI agent commands.",
        "icons": { "48": "icons/icon-48.png" },
        "background": { "scripts": ["background.js"] },
        "browser_action": {},
        "browser_specific_settings": { "gecko": { "id": "x@y" } }
    });
    let mv3 = build_mv3_manifest(&orig);

    assert_eq!(mv3["manifest_version"], 3);
    assert_eq!(mv3["version"], "0.9.7");
    assert_eq!(mv3["key"], CHROME_EXTENSION_KEY);
    assert_eq!(mv3["background"]["service_worker"], "sw.js");
    // Firefox-only keys must not leak through.
    assert!(mv3.get("browser_action").is_none());
    assert!(mv3.get("browser_specific_settings").is_none());
    // MV3 moves broad host access out of permissions.
    let perms = mv3["permissions"].as_array().expect("permissions");
    assert!(!perms.iter().any(|p| p == "<all_urls>"));
    assert_eq!(mv3["host_permissions"][0], "<all_urls>");
    // The shim must load before the Firefox-flavored content script.
    assert_eq!(
        mv3["content_scripts"][0]["js"],
        serde_json::json!(["browser-shim.js", "content.js"])
    );
}

#[test]
fn chrome_paths_live_under_browser_dir() {
    let _guard = crate::storage::lock_test_env();
    let ext = chrome_extension_dir();
    assert!(ext.to_string_lossy().contains("browser"));
    assert!(ext.to_string_lossy().ends_with("chrome-extension"));
}

#[test]
fn extension_id_matches_key_derivation() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let der = base64::engine::general_purpose::STANDARD
        .decode(CHROME_EXTENSION_KEY)
        .expect("key is valid base64");
    let digest = Sha256::digest(&der);
    let derived: String = digest[..16]
        .iter()
        .flat_map(|b| [b >> 4, b & 0xf])
        .map(|nibble| (b'a' + nibble) as char)
        .collect();
    assert_eq!(derived, CHROME_EXTENSION_ID);
}

#[test]
fn shim_wraps_promise_listeners_and_aliases_browser_action() {
    assert!(BROWSER_SHIM_JS.contains("globalThis.browser"));
    assert!(BROWSER_SHIM_JS.contains("browserAction"));
    assert!(BROWSER_SHIM_JS.contains("typeof result.then === \"function\""));
}

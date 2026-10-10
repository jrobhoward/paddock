#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn check_key_permissions____missing_file____error() {
    let dir = tempfile::tempdir().unwrap();

    let result = check_key_permissions(&dir.path().join("absent.key"));

    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
}

#[cfg(unix)]
#[rstest::rstest]
#[case(0o600, true)]
#[case(0o400, true)]
#[case(0o700, true)]
#[case(0o640, false)]
#[case(0o604, false)]
#[case(0o644, false)]
#[case(0o620, false)]
fn check_key_permissions____mode____accepted_only_when_owner_only(
    #[case] mode: u32,
    #[case] accepted: bool,
) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.key");
    std::fs::write(&path, "key").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();

    let result = check_key_permissions(&path);

    assert_eq!(result.is_ok(), accepted, "{result:?}");
    if let Err(e) = result {
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("chmod 600"), "{e}");
    }
}

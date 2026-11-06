use super::opaque_id;

#[test]
fn opaque_ids_have_expected_entropy_length_and_hex_format() {
    let id = opaque_id(32);
    assert_eq!(id.len(), 64);
    assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(id, opaque_id(32));
}

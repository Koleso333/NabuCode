// The window reads the version from the TS mirror, so both sides have to name the same number.
#[test]
fn ts_mirror_has_the_same_protocol_version() {
    let ts = include_str!("../ts/src/index.ts");
    let line = format!("export const protocolVersion = {}\n", nabu_protocol::PROTOCOL_VERSION);
    assert!(ts.replace("\r\n", "\n").contains(&line), "update protocolVersion in ts/src/index.ts");
}

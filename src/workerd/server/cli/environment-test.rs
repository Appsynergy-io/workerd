use super::parse;

#[test]
fn records_are_name_value_pairs_each_ended_by_nul() {
    let records = parse(b"A=1\0B=x=y\0EMPTY=\0").unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records["A"], "1");
    // Only the first '=' ends the name.
    assert_eq!(records["B"], "x=y");
    assert_eq!(records["EMPTY"], "");
    assert!(parse(b"").unwrap().is_empty());
}

#[test]
fn a_malformed_record_is_refused() {
    for (bytes, error) in [
        (&b"A=1"[..], "not ended by a NUL byte"),
        (b"A\0", "no '='"),
        (b"=1\0", "empty name"),
        (b"A=1\0A=2\0", "given twice: A"),
        (b"A=\xff\0", "not UTF-8"),
    ] {
        let refusal = parse(bytes).unwrap_err();
        assert!(refusal.contains(error), "{bytes:?}: {refusal}");
    }
}

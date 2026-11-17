//! The diagnostic code registry is complete, and ERRORS.md is generated from it.

#[test]
fn every_code_in_the_source_is_registered_and_every_registered_code_is_used() {
    let mut used = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir("src").unwrap() {
        let p = entry.unwrap().path();
        if p.extension().map_or(true, |x| x != "rs") || p.ends_with("codes.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap();
        let b = text.as_bytes();
        for i in 0..b.len().saturating_sub(4) {
            if b[i] == b'K' && b[i + 1..i + 5].iter().all(|c| c.is_ascii_digit()) && (i + 5 >= b.len() || !b[i + 5].is_ascii_digit()) {
                used.insert(text[i..i + 5].to_string());
            }
        }
    }
    let registered: std::collections::BTreeSet<String> = kiln::codes::CODES.iter().map(|c| c.code.to_string()).collect();
    let missing: Vec<_> = used.difference(&registered).collect();
    let unused: Vec<_> = registered.difference(&used).collect();
    assert!(missing.is_empty(), "codes used but not in src/codes.rs: {:?}", missing);
    assert!(unused.is_empty(), "codes in src/codes.rs that nothing reports: {:?}", unused);
}

#[test]
fn errors_md_is_current() {
    let want = kiln::codes::markdown();
    let got = std::fs::read_to_string("ERRORS.md").unwrap_or_default();
    if got != want {
        if std::env::var("KILN_BLESS").is_ok() {
            std::fs::write("ERRORS.md", &want).unwrap();
        } else {
            panic!("ERRORS.md is stale: regenerate it with `KILN_BLESS=1 cargo test --test codes`");
        }
    }
}

//! Committed output must match what the compiler produces now. This is the
//! same check as `kiln build --check`, for every example.

#[test]
fn examples_match_committed_output() {
    let mut checked = 0;
    for entry in std::fs::read_dir("examples").unwrap() {
        let dir = entry.unwrap().path();
        let src_path = dir.join("main.kiln");
        if !src_path.exists() {
            continue;
        }
        let shown = src_path.to_string_lossy().replace('\\', "/");
        let src = std::fs::read_to_string(&src_path).unwrap();
        let lock = std::fs::read_to_string(dir.join("kiln.lock")).ok();
        let opts = kiln::BuildOpts {
            lock: lock.as_deref(),
            schemas: Some(std::rc::Rc::new(kiln::schema::SchemaSet::from_dir(".kiln/schemas".into()))),
            strict: true,
            ..Default::default()
        };
        let b = kiln::build(&src, &shown, &opts).unwrap_or_else(|e| panic!("{}", e));
        assert!(b.warnings.is_empty(), "{}", b.warnings.join("\n"));
        assert_eq!(Some(b.lock.as_str()), lock.as_deref(), "{}: kiln.lock is stale", shown);
        let got = b.json;
        let want = std::fs::read_to_string(dir.join("out/main.tf.json"))
            .unwrap_or_else(|_| panic!("{}: no committed out/main.tf.json", shown));
        assert_eq!(got, want, "{}: committed output is stale; run `kiln build {}`", shown, shown);
        checked += 1;
    }
    assert!(checked > 0);
}

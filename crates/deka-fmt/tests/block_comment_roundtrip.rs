#[test]
fn preserves_block_comments_through_round_trip() {
    let input = include_str!("fixtures/block_comments.ds");

    let once = deka_fmt::format_ds(input).expect("formats block comment fixture");
    let twice = deka_fmt::format_ds(&once).expect("formats block comment fixture twice");

    assert_eq!(
        once, input,
        "first format must preserve every block comment"
    );
    assert_eq!(twice, input, "second format must remain byte-identical");
}

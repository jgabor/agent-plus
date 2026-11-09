//! What guest output may and may not reach the user's terminal.

use ap::filter::Filter;

fn filtered(input: &[u8]) -> Vec<u8> {
    Filter::default().push(input)
}

/// The result must not depend on where the guest's frames happen to split.
#[track_caller]
fn assert_filters(input: &[u8], expected: &[u8]) {
    assert_eq!(filtered(input), expected, "whole: {input:?}");
    let mut filter = Filter::default();
    let mut bytes = Vec::new();
    for byte in input {
        bytes.extend(filter.push(&[*byte]));
    }
    assert_eq!(bytes, expected, "byte by byte: {input:?}");
    for split in 1..input.len() {
        let mut filter = Filter::default();
        let mut bytes = filter.push(&input[..split]);
        bytes.extend(filter.push(&input[split..]));
        assert_eq!(bytes, expected, "split at {split}: {input:?}");
    }
}

#[track_caller]
fn assert_passes(input: &[u8]) {
    assert_filters(input, input);
}

#[test]
fn plain_output_and_utf8_pass_unchanged() {
    assert_passes(b"hello\r\n\tworld\x07\n");
    assert_passes("ŝ€𝄞 é".as_bytes());
}

#[test]
fn utf8_whose_continuation_bytes_look_like_controls_passes() {
    // U+015D ends in 0x9D, and U+0150 in 0x90, the 8-bit OSC and DCS.
    assert_passes("ŝ52;c;aGk=\x07".as_bytes());
    assert_passes("Ő$q".as_bytes());
}

#[test]
fn clipboard_sequences_are_removed() {
    assert_filters(b"a\x1b]52;c;aGk=\x07b", b"ab");
    assert_filters(b"a\x1b]52;c;aGk=\x1b\\b", b"ab");
    assert_filters(b"a\x1b]52;c;?\x07b", b"ab");
    assert_filters(b"a\x1b]052;c;aGk=\x07b", b"ab");
}

#[test]
fn clipboard_sequences_in_c1_form_are_removed() {
    assert_filters(b"a\x9d52;c;aGk=\x07b", b"ab");
    assert_filters(b"a\xc2\x9d52;c;aGk=\xc2\x9cb", b"ab");
    assert_filters(b"a\x1b]52;c;aGk=\x9cb", b"ab");
}

#[test]
fn a_cancelled_clipboard_sequence_ends_where_the_terminal_ends_it() {
    assert_filters(b"a\x1b]52;c;aGk=\x18b", b"ab");
    assert_filters(b"a\x1b]52;c;aGk=\x1ab", b"ab");
    assert_filters(b"a\x1b]52;c;aGk=\x1b[1mb", b"a\x1b[1mb");
}

#[test]
fn a_clipboard_sequence_of_any_length_is_removed() {
    let mut input = b"a\x1b]52;c;".to_vec();
    input.resize(1 << 20, b'A');
    input.extend(b"\x07b");
    assert_eq!(filtered(&input), b"ab");
}

#[test]
fn a_control_inside_a_sequence_does_not_hide_it() {
    assert_filters(b"\x1b]5\n2;c;aGk=\x07b", b"b");
    assert_filters(b"\x1b\n[21t", b"");
    assert_filters(b"\x1b[2\n1t", b"");
    assert_filters(b"\x1bP\n$qm\x1b\\b", b"b");
}

#[test]
fn title_reports_are_removed() {
    assert_filters(b"a\x1b[21tb", b"ab");
    assert_filters(b"a\x1b[20tb", b"ab");
    assert_filters(b"a\x1b[021;0tb", b"ab");
    assert_filters(b"a\x9b21tb", b"ab");
    assert_filters(b"a\xc2\x9b21tb", b"ab");
}

#[test]
fn setting_a_status_request_echo_is_removed() {
    assert_filters(b"a\x1bP$qm\x1b\\b", b"ab");
    assert_filters(b"a\x901$q\"p\x9cb", b"ab");
}

#[test]
fn queries_for_the_terminals_own_state_pass() {
    assert_passes(b"\x1b[6n");
    assert_passes(b"\x1b[c");
    assert_passes(b"\x1b[>c");
    assert_passes(b"\x1b[?6n");
    assert_passes(b"\x1b[18t");
    assert_passes(b"\x1b[14t");
    assert_passes(b"\x1b]11;?\x07");
    assert_passes(b"\x1b]10;?\x1b\\");
    assert_passes(b"\x1b]4;1;?\x07");
}

#[test]
fn other_sequences_pass() {
    assert_passes(b"\x1b[1;31mred\x1b[0m");
    assert_passes(b"\x1b]0;a title\x07");
    assert_passes(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");
    assert_passes(b"\x1b]520;x\x07");
    assert_passes(b"\x1bP1;2|x\x1b\\");
    assert_passes(b"\x1bc");
    assert_passes(b"\x1b(B");
    assert_passes(b"\x1b_kitty\x1b\\");
}

#[test]
fn a_removed_sequence_inside_a_passed_string_is_still_removed() {
    assert_filters(b"\x1b]0;a\x1b]52;c;aGk=\x07b", b"\x1b]0;a\x1b\\b");
}

#[test]
fn removing_a_sequence_closes_a_string_it_would_have_ended() {
    assert_filters(b"\x1b]0;a\x1b[21tb", b"\x1b]0;a\x1b\\b");
    assert_filters(b"\x1bP1|a\x1bP$qm\x1b\\b", b"\x1bP1|a\x1b\\b");
    assert_filters(b"\x1b]0;a\x1b[1m\x1b[21tb", b"\x1b]0;a\x1b[1m\x1b\\b");
    // BEL ends an OSC but not a DCS, so it never counts as an end.
    assert_filters(b"\x1bP1|a\x07\x1b[21tb", b"\x1bP1|a\x07\x1b\\b");
    assert_filters(b"\x1b_a\x1b[21tb", b"\x1b_a\x1b\\b");
    assert_filters(b"\x98a\x1b[21tb", b"\x98a\x1b\\b");
}

#[test]
fn a_string_already_ended_is_not_closed_again() {
    assert_filters(b"\x1b]0;a\x1b\\\x1b[21tb", b"\x1b]0;a\x1b\\b");
    assert_filters(b"\x1b]0;a\x18\x1b[21tb", b"\x1b]0;a\x18b");
}

#[test]
fn a_clipboard_code_padded_with_zeros_is_removed() {
    // Some terminals read a fixed number of digits, and take the rest as data.
    assert_filters(b"a\x1b]000520c;?\x07b", b"ab");
}

#[test]
fn kittys_clipboard_protocol_is_removed() {
    assert_filters(b"a\x1b]5522;type=read\x07b", b"ab");
}

#[test]
fn an_unfinished_sequence_is_never_written() {
    // Whatever is printed next, a shell prompt or the other stream, must not
    // complete what the guest started.
    for start in [
        &b"\x1b"[..],
        b"\x1b[",
        b"\x1b[2",
        b"\x1b]",
        b"\x1b]5",
        b"\x1bP",
        b"\x1bP$",
        b"\x9b2",
        b"\xc2",
    ] {
        assert_eq!(filtered(start), b"", "{start:?}");
    }
}

#[test]
fn a_sequence_cut_short_by_another_is_not_written() {
    assert_filters(b"\x1b[2\x1b[1mb", b"\x1b[1mb");
    assert_filters(b"\x1b\x1b[1mb", b"\x1b[1mb");
    assert_filters(b"\x1b[2\x18b", b"\x18b");
}

#[test]
fn an_oversized_sequence_is_dropped_without_holding_it_all() {
    let mut input = b"\x1b[".to_vec();
    input.resize(1 << 20, b'1');
    input.extend(b"mb");
    assert_eq!(filtered(&input), b"b");
}

#[test]
fn state_lives_in_the_filter_so_each_stream_gets_its_own() {
    let mut stdout = Filter::default();
    let mut stderr = Filter::default();
    assert_eq!(stdout.push(b"\x1b[2"), b"");
    assert_eq!(stderr.push(b"1t"), b"1t");
    assert_eq!(stdout.push(b"1t"), b"");
}

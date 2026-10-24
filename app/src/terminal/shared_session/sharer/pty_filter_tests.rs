use super::SharedPtyFilter;

const PREEXEC: &[u8] = b"\x1bP$d7b22686f6f6b223a225072656578656322\x1b\\";
const FINISHED: &[u8] = b"\x1bP$d7b22686f6f6b223a22436f6d6d616e6446696e697368656422\x1b\\";

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn passes_everything_through_outside_in_band_commands() {
    let bytes = concat(&[b"ls\r\n", PREEXEC, b"file.txt\r\n", FINISHED]);
    assert_eq!(SharedPtyFilter::default().filter(&bytes, false), bytes);
}

#[test]
fn keeps_only_hooks_during_in_band_commands() {
    let bytes = concat(&[
        b"warp_run_generator_command_native_completions 6364\r\n",
        PREEXEC,
        b"\x1b]9278;d;payload\x07",
        FINISHED,
        b"\x1b[J",
    ]);
    assert_eq!(
        SharedPtyFilter::default().filter(&bytes, true),
        concat(&[PREEXEC, FINISHED])
    );
}

#[test]
fn keeps_a_hook_split_across_reads() {
    let mut filter = SharedPtyFilter::default();
    let (head, tail) = PREEXEC.split_at(10);
    let first = filter.filter(&concat(&[b"echo", head]), true);
    let second = filter.filter(&concat(&[tail, b"output"]), true);
    assert_eq!(concat(&[&first, &second]), PREEXEC);
}

#[test]
fn does_not_repeat_a_hook_start_split_between_modes() {
    let mut filter = SharedPtyFilter::default();
    let (head, tail) = PREEXEC.split_at(2);
    let first = filter.filter(&concat(&[b"prompt", head]), false);
    let second = filter.filter(tail, true);
    assert_eq!(concat(&[&first, &second]), concat(&[b"prompt", PREEXEC]));
}

#[test]
fn escape_sequences_that_are_not_hooks_are_dropped_in_band() {
    let bytes = b"\x1b[31mred\x1b[0m\x1bP+q\x1b\\";
    assert!(SharedPtyFilter::default().filter(bytes, true).is_empty());
}

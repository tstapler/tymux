use std::sync::mpsc;
use std::time::Duration;

use super::scanner::QueryScanner;
use super::*;

struct Collect(mpsc::Sender<Vec<u8>>);

impl ReplyWriter for Collect {
    fn write_reply(&mut self, bytes: &[u8]) {
        let _ = self.0.send(bytes.to_vec());
    }
}

struct Harness {
    parser: vt100::Parser<QueryResponder>,
    replies: Arc<QueryReplies>,
    rx: mpsc::Receiver<Vec<u8>>,
}

impl Harness {
    fn new(rows: u16, cols: u16, colors: Colors) -> Self {
        let (tx, rx) = mpsc::channel();
        let replies = Arc::new(QueryReplies::new(Box::new(Collect(tx)), colors));
        let parser =
            vt100::Parser::new_with_callbacks(rows, cols, 0, QueryResponder::new(replies.clone()));
        Self {
            parser,
            replies,
            rx,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> &mut Self {
        self.parser.process(bytes);
        self
    }

    /// Replies written so far, with ESC spelled out.
    fn replies(&self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(r) = self.rx.recv_timeout(Duration::from_millis(100)) {
            out.push(
                String::from_utf8_lossy(&r)
                    .replace('\x1b', "ESC")
                    .replace('\x07', "BEL"),
            );
        }
        out
    }
}

fn replies_to(input: &[u8]) -> Vec<String> {
    Harness::new(10, 40, Colors::default())
        .feed(input)
        .replies()
}

#[test]
fn da1_should_report_vt100_with_advanced_video() {
    assert_eq!(replies_to(b"\x1b[c"), ["ESC[?1;2c"]);
    assert_eq!(replies_to(b"\x1b[0c"), ["ESC[?1;2c"]);
}

#[test]
fn da2_should_report_secondary_attributes() {
    assert_eq!(replies_to(b"\x1b[>c"), ["ESC[>0;276;0c"]);
}

#[test]
fn dsr_5_should_report_ok() {
    assert_eq!(replies_to(b"\x1b[5n"), ["ESC[0n"]);
}

#[test]
fn dsr_6_should_report_the_real_cursor_position() {
    assert_eq!(replies_to(b"\x1b[6n"), ["ESC[1;1R"]);
    assert_eq!(replies_to(b"ab\r\n  xyz\x1b[6n"), ["ESC[2;6R"]);
    assert_eq!(replies_to(b"\x1b[?6n"), ["ESC[?1;1;1R"]);
}

#[test]
fn dsr_6_should_clamp_a_pending_wrap_to_the_last_column() {
    let mut h = Harness::new(5, 40, Colors::default());
    h.feed(b"\x1b[1;40Hx\x1b[6n");
    assert_eq!(h.replies(), ["ESC[1;40R"]);
}

#[test]
fn dsr_6_should_be_relative_to_the_scroll_region_under_origin_mode() {
    assert_eq!(
        replies_to(b"\x1b[3;8r\x1b[?6h\x1b[2;4H\x1b[6n"),
        ["ESC[2;4R"]
    );
}

#[test]
fn xtversion_should_report_tymux() {
    let got = replies_to(b"\x1b[>q");
    assert_eq!(got.len(), 1);
    assert!(
        got[0].starts_with("ESCP>|tymux ") && got[0].ends_with("ESC\\"),
        "{got:?}"
    );
    assert_eq!(replies_to(b"\x1b[>0q"), got);
}

#[test]
fn decrqm_should_report_set_reset_permanent_and_unknown_modes() {
    assert_eq!(
        replies_to(b"\x1b[?25$p\x1b[?25l\x1b[?25$p\x1b[?2004$p\x1b[?2004h\x1b[?2004$p\x1b[?9999$p"),
        [
            "ESC[?25;1$y",
            "ESC[?25;2$y",
            "ESC[?2004;2$y",
            "ESC[?2004;1$y",
            "ESC[?9999;0$y"
        ]
    );
    assert_eq!(
        replies_to(b"\x1b[4$p\x1b[4h\x1b[4$p"),
        ["ESC[4;2$y", "ESC[4;1$y"]
    );
    assert_eq!(
        replies_to(b"\x1b[2$p\x1b[12$p\x1b[99$p"),
        ["ESC[2;4$y", "ESC[12;3$y", "ESC[99;0$y"]
    );
    assert_eq!(
        replies_to(b"\x1b[?8$p\x1b[?67$p\x1b[?1005$p\x1b[?1015$p\x1b[?1048$p"),
        [
            "ESC[?8;3$y",
            "ESC[?67;4$y",
            "ESC[?1005;4$y",
            "ESC[?1015;4$y",
            "ESC[?1048;1$y"
        ]
    );
    assert_eq!(
        replies_to(b"\x1b[?1000h\x1b[?1006h\x1b[?1000$p\x1b[?1006$p\x1b[?1003$p"),
        ["ESC[?1000;1$y", "ESC[?1006;1$y", "ESC[?1003;2$y"]
    );
}

#[test]
fn xtwinops_should_report_the_size_in_characters() {
    assert_eq!(replies_to(b"\x1b[18t"), ["ESC[8;10;40t"]);
    assert_eq!(replies_to(b"\x1b[19t"), ["ESC[9;10;40t"]);
}

#[test]
fn osc_color_queries_should_report_the_default_palette_with_the_querys_terminator() {
    assert_eq!(
        replies_to(b"\x1b]10;?\x07"),
        ["ESC]10;rgb:ffff/ffff/ffffBEL"]
    );
    assert_eq!(
        replies_to(b"\x1b]11;?\x1b\\"),
        ["ESC]11;rgb:0000/0000/0000ESC\\"]
    );
    assert_eq!(
        replies_to(b"\x1b]12;?\x07"),
        ["ESC]12;rgb:ffff/ffff/ffffBEL"]
    );
    assert_eq!(
        replies_to(b"\x1b]4;1;?\x07"),
        ["ESC]4;1;rgb:cccc/0000/0000BEL"]
    );
    assert_eq!(
        replies_to(b"\x1b]4;232;?\x1b\\"),
        ["ESC]4;232;rgb:0808/0808/0808ESC\\"]
    );
    assert_eq!(
        replies_to(b"\x1b]4;196;?\x07"),
        ["ESC]4;196;rgb:ffff/0000/0000BEL"]
    );
}

#[test]
fn osc_10_with_several_question_marks_should_answer_each_dynamic_colour() {
    assert_eq!(
        replies_to(b"\x1b]10;?;?;?\x07"),
        [
            "ESC]10;rgb:ffff/ffff/ffffBEL",
            "ESC]11;rgb:0000/0000/0000BEL",
            "ESC]12;rgb:ffff/ffff/ffffBEL"
        ]
    );
    // a set mixed into the query makes it a non-query, left for the client
    assert!(replies_to(b"\x1b]11;#123456;?\x07").is_empty());
}

#[test]
fn osc_colour_replies_should_use_the_configured_colours() {
    let colors = Colors::from_lookup(|k| match k {
        "TYMUXD_QUERY_FG" => Some("#102030".into()),
        "TYMUXD_QUERY_BG" => Some("#fafafa".into()),
        "TYMUXD_QUERY_CURSOR" => Some("nonsense".into()),
        _ => None,
    });
    let mut h = Harness::new(5, 20, colors);
    h.feed(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07");
    assert_eq!(
        h.replies(),
        [
            "ESC]10;rgb:1010/2020/3030BEL",
            "ESC]11;rgb:fafa/fafa/fafaBEL",
            "ESC]12;rgb:ffff/ffff/ffffBEL"
        ]
    );
}

#[test]
fn osc_setting_a_color_should_not_be_answered() {
    assert!(replies_to(b"\x1b]10;#ff0000\x07\x1b]4;1;#00ff00\x07").is_empty());
}

#[test]
fn kitty_keyboard_query_should_not_be_answered() {
    assert!(replies_to(b"\x1b[?u").is_empty());
}

#[test]
fn non_query_sequences_should_not_be_answered() {
    assert!(replies_to(b"\x1b[2J\x1b[1;31mhi\x1b[0m\x1b[8;10;40t\x1b[>1c\x1b[7n").is_empty());
}

#[test]
fn a_da1_inside_a_dcs_payload_should_not_be_answered() {
    assert!(replies_to(b"\x1bPq[c6n\x1b\\").is_empty());
}

#[test]
fn an_oversized_parameter_should_not_be_answered() {
    assert!(replies_to(b"\x1b[65536c").is_empty());
}

#[test]
fn a_query_split_across_chunks_should_be_answered_exactly_once() {
    let mut h = Harness::new(10, 40, Colors::default());
    h.feed(b"\x1b[").feed(b"6").feed(b"n");
    assert_eq!(h.replies(), ["ESC[1;1R"]);
}

#[test]
fn nothing_should_be_answered_while_a_client_answers() {
    let mut h = Harness::new(10, 40, Colors::default());
    h.replies.set_daemon_answers(false);
    h.feed(b"\x1b[6n\x1b[c");
    assert!(h.replies().is_empty());
    h.replies.set_daemon_answers(true);
    h.feed(b"\x1b[c");
    assert_eq!(h.replies(), ["ESC[?1;2c"]);
}

#[test]
fn a_burst_of_identical_queries_should_not_lose_the_reply() {
    let got = replies_to("\x1b[c".repeat(20).as_bytes());
    assert!(
        !got.is_empty() && got.iter().all(|r| r == "ESC[?1;2c"),
        "{got:?}"
    );
}

#[test]
fn strip_and_reply_should_agree_on_what_is_a_query() {
    for seq in [
        &b"\x1b[c"[..],
        b"\x1b[0c",
        b"\x1b[0:1c",
        b"\x1b[>c",
        b"\x1b[5n",
        b"\x1b[6n",
        b"\x1b[?6n",
        b"\x1b[>q",
        b"\x1b[4$p",
        b"\x1b[?25$p",
        b"\x1b[18t",
        b"\x1b]10;?\x07",
        b"\x1b]10;?;?\x07",
        b"\x1b]4;3;?\x1b\\",
        b"\x1b]4;3;#fff\x07",
        b"\x1b[7n",
        b"\x1b[?u",
        b"\x1b[8;5;5t",
        b"\x1b[65536c",
        b"\x1b[1;2c",
        b"\x1b[0;5c",
        b"\x1b[6;1n",
        b"\x1bPq[c\x1b\\",
        b"\x1b[6\nn",
    ] {
        let answered = !replies_to(seq).is_empty();
        let mut scanner = QueryScanner::default();
        let out = scanner.feed(seq);
        let stripped = out.stripped().len() < out.raw.len();
        assert_eq!(answered, stripped, "{:?}", String::from_utf8_lossy(seq));
    }
}

#[test]
fn an_osc4_query_should_answer_exactly_the_pairs_vte_kept() {
    // vte keeps 16 OSC parameters: "4" plus 7 pairs (and half of an eighth, which is dropped)
    let mut q = String::from("\x1b]4");
    for i in 0..40 {
        q.push_str(&format!(";{i};?"));
    }
    q.push('\x07');
    let got = replies_to(q.as_bytes());
    assert_eq!(got.len(), 7, "{got:?}");
    assert_eq!(got[0], "ESC]4;0;rgb:2e2e/3434/3636BEL");
    assert_eq!(got[6], "ESC]4;6;rgb:0606/9898/9a9aBEL");
}

#[test]
fn an_osc_mixing_sets_and_queries_should_not_be_answered() {
    assert!(replies_to(b"\x1b]4;1;rgb:ff/00/00;2;?\x07").is_empty());
    assert!(replies_to(b"\x1b]10;?;#00f\x07").is_empty());
}

#[test]
fn a_c0_control_inside_a_csi_should_not_stop_the_query_being_answered() {
    assert_eq!(replies_to(b"\x1b[6\nn"), ["ESC[2;1R"]);
}

/// What the real responder would do, minus the writer thread, so thousands of cases run fast.
#[derive(Default)]
struct Recorder {
    answered: usize,
}

impl Callbacks for Recorder {
    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        p: &[&[u16]],
        c: char,
    ) {
        self.answered += usize::from(csi_reply(screen, i1, i2, p, c).is_some());
    }
    fn unhandled_osc_terminated(&mut self, _: &mut Screen, params: &[&[u8]], bel: bool) {
        self.answered += usize::from(!osc_replies(params, bel, &Colors::default()).is_empty());
    }
}

fn answered_by_parser(bytes: &[u8]) -> bool {
    let mut parser = vt100::Parser::new_with_callbacks(10, 40, 0, Recorder::default());
    parser.process(bytes);
    parser.callbacks().answered > 0
}

mod differential {
    use super::*;
    use proptest::prelude::*;

    /// Self-contained, terminated items; each is a query or not purely by what the parser does.
    fn item() -> impl Strategy<Value = Vec<u8>> {
        let csi_final = prop::sample::select(b"cnqptmhlJHr@u".to_vec());
        let private = prop::sample::select(vec![&b""[..], b"?", b">", b"=", b"$", b"?$", b" "]);
        let params = prop::sample::select(vec![
            &b""[..],
            b"0",
            b"5",
            b"6",
            b"18",
            b"19",
            b"7",
            b"65536",
            b"99999",
            b"0:1",
            b"1;2",
            b"0;5",
            b"6;1",
            b"25",
            b"2004",
            b"8;5;5",
        ]);
        let csi = (private, params, csi_final).prop_map(|(pre, p, f)| {
            let mut v = b"\x1b[".to_vec();
            v.extend_from_slice(pre);
            v.extend_from_slice(p);
            v.push(f);
            v
        });
        let osc_body = prop::sample::select(vec![
            &b"10;?"[..],
            b"11;?",
            b"12;?",
            b"10;?;?",
            b"10;#fff",
            b"10;?;#00f",
            b"4;1;?",
            b"4;1;?;2;?",
            b"4;1;rgb:ff/00/00;2;?",
            b"4;300;?",
            b"4;1;#fff",
            b"0;title",
            b"52;c;?",
            b"8;;http://x",
            b"4;1",
            b"4;1;?;2",
        ]);
        let osc_end = prop::sample::select(vec![&b"\x07"[..], b"\x1b\\"]);
        let osc = (osc_body, osc_end).prop_map(|(b, e)| {
            let mut v = b"\x1b]".to_vec();
            v.extend_from_slice(b);
            v.extend_from_slice(e);
            v
        });
        let c0_csi = prop::sample::select(vec![&b"\x1b[6\nn"[..], b"\x1b[\x076n", b"\x1b[5\rn"])
            .prop_map(<[u8]>::to_vec);
        let dcs = prop::sample::select(vec![&b"\x1bPq[c6n\x1b\\"[..], b"\x1b_apc[c\x1b\\"])
            .prop_map(<[u8]>::to_vec);
        let text = "[a-z \\n]{0,6}".prop_map(String::into_bytes);
        prop_oneof![5 => csi, 4 => osc, 1 => c0_csi, 1 => dcs, 3 => text]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 3000, ..ProptestConfig::default() })]

        /// Stripping removes exactly the items the parser answers (keeping C0 controls the
        /// terminal would act on), however the stream is cut into chunks.
        #[test]
        fn strip_should_remove_exactly_what_the_parser_answers(
            items in prop::collection::vec(item(), 1..12),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..6),
        ) {
            let raw: Vec<u8> = items.concat();
            let mut expected = Vec::new();
            for it in &items {
                if !answered_by_parser(it) {
                    expected.extend_from_slice(it);
                } else if it.starts_with(b"\x1b[") {
                    // C0 controls inside a removed CSI still act on the terminal
                    expected.extend(it[1..].iter().copied().filter(|b| *b < 0x20));
                } else if it.ends_with(b"\x1b\\") {
                    // an ESC terminator may start the next sequence, so it stays
                    expected.extend_from_slice(b"\x1b\\");
                }
            }
            let mut points: Vec<usize> = cuts.iter().map(|c| c.index(raw.len() + 1)).collect();
            points.sort_unstable();
            points.push(raw.len());
            let mut scanner = QueryScanner::default();
            let (mut got_raw, mut got) = (Vec::new(), Vec::new());
            let mut at = 0;
            for p in points {
                let out = scanner.feed(&raw[at..p]);
                got_raw.extend_from_slice(&out.raw);
                got.extend_from_slice(out.stripped());
                at = p;
            }
            // anything still held belongs to a never-terminated tail; items are all terminated
            prop_assert_eq!(&got_raw, &raw);
            prop_assert_eq!(got, expected);
        }
    }
}

/// Overhead of the stream scanner on SGR-heavy output. Run with
/// `cargo test --release -p tymux-core --lib scanner_overhead -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn scanner_overhead_on_sgr_heavy_output() {
    fn thread_cpu_secs() -> f64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid pointer to a timespec.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
    }
    let line = b"\x1b[1;32mfoo\x1b[0m bar \x1b[38;5;208mbaz\x1b[0m qux \x1b[7mrev\x1b[27m\r\n";
    let data: Vec<u8> = line.iter().copied().cycle().take(40 << 20).collect();
    let replies = {
        let (tx, _rx) = mpsc::channel();
        Arc::new(QueryReplies::new(Box::new(Collect(tx)), Colors::default()))
    };
    let scanner_only = {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let mut scanner = QueryScanner::default();
            let start = thread_cpu_secs();
            for chunk in data.chunks(4096) {
                std::hint::black_box(scanner.feed(chunk).stripped().len());
            }
            best = best.min(thread_cpu_secs() - start);
        }
        40.0 / best
    };
    let best = |with_scanner: bool| {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let mut parser =
                vt100::Parser::new_with_callbacks(50, 200, 0, QueryResponder::new(replies.clone()));
            let mut scanner = QueryScanner::default();
            let start = thread_cpu_secs();
            for chunk in data.chunks(4096) {
                parser.process(chunk);
                if with_scanner {
                    std::hint::black_box(scanner.feed(chunk).stripped().len());
                }
            }
            best = best.min(thread_cpu_secs() - start);
        }
        40.0 / best
    };
    let without = best(false);
    let with = best(true);
    println!("scanner only: {scanner_only:.1} MB/s");
    println!(
        "parser only: {without:.1} MB/s; parser+scanner: {with:.1} MB/s; overhead {:.1}%",
        (without / with - 1.0) * 100.0
    );
}

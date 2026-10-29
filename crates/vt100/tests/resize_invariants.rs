//! Randomized resize torture: no panic, and the screen's shape invariants hold after every resize.
//! Deterministic (seeded LCG) so a failure reproduces from the printed seed.
use vt100::Parser;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const SNIPPETS: &[&str] = &[
    "hello world ",
    "0123456789",
    "世界你好",
    "😀",
    "e\u{301}",
    "\r\n",
    "\r\n",
    "\x1b[31m",
    "\x1b[0m",
    "\x1b[1;4;48;5;17m",
    "\x1b[2J\x1b[H",
    "\x1b[3;7r",
    "\x1b[r",
    "\x1b7",
    "\x1b8",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b]8;;http://e.x/\x07link\x1b]8;;\x07",
    "\t",
    "\x1b[5A",
    "\x1b[3B",
    "\x1b[2L",
    "\x1b[2M",
    "\x1b[2@",
    "\x1b[?7l",
    "\x1b[?7h",
];

fn assert_shape(p: &mut Parser, seed: u64, step: usize) {
    let (rows, cols) = p.screen().size();
    let ctx = format!("seed {seed} step {step} size {rows}x{cols}");
    for r in 0..rows {
        for c in 0..cols {
            assert!(
                p.screen().cell(r, c).is_some(),
                "missing cell r{r} c{c}: {ctx}"
            );
        }
    }
    let (cr, cc) = p.screen().cursor_position();
    assert!(
        cr < rows && cc <= cols,
        "cursor out of range ({cr},{cc}): {ctx}"
    );
    let (top, bottom) = p.screen().scroll_region();
    // 1-based, or (0, 0) for the whole screen
    assert!(
        (top, bottom) == (0, 0) || (1 <= top && top < bottom && bottom <= rows),
        "bad scroll region ({top},{bottom}): {ctx}"
    );

    p.screen_mut().set_scrollback(usize::MAX);
    let sb = p.screen().scrollback();
    for off in (1..=sb).step_by((sb / 40).max(1)) {
        p.screen_mut().set_scrollback(off);
        for c in 0..cols {
            assert!(
                p.screen().cell(0, c).is_some(),
                "scrollback row {off} narrower than {cols}: {ctx}"
            );
        }
    }
    p.screen_mut().set_scrollback(0);
    for r in 0..rows {
        for c in 0..cols {
            let cell = p.screen().cell(r, c).unwrap();
            assert!(
                !(cell.is_wide()
                    && (c + 1 == cols
                        || !p.screen().cell(r, c + 1).unwrap().is_wide_continuation())),
                "wide cell without its continuation at r{r} c{c}: {ctx}"
            );
        }
    }
}

#[test]
fn random_writes_and_resizes_keep_the_screen_well_formed() {
    for seed in 0..100u64 {
        let mut rng = Lcg(seed + 1);
        let scrollback = [0usize, 3, 50, 1000][rng.below(4) as usize];
        let mut p = Parser::new(
            2 + rng.below(20) as u16,
            2 + rng.below(60) as u16,
            scrollback,
        );
        for step in 0..60 {
            if rng.below(3) == 0 {
                p.screen_mut()
                    .set_size(1 + rng.below(30) as u16, 1 + rng.below(90) as u16);
                assert_shape(&mut p, seed, step);
            } else {
                for _ in 0..1 + rng.below(8) {
                    p.process(SNIPPETS[rng.below(SNIPPETS.len() as u64) as usize].as_bytes());
                }
            }
        }
        assert_shape(&mut p, seed, 999);
    }
}

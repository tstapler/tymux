//! Printing text must not allocate per character (the fork once built a `String` for every
//! printed character to support REP). A counting allocator makes that deterministic.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static A: Counting = Counting;

fn allocs() -> usize {
    ALLOCS.with(Cell::get)
}

#[test]
fn printing_ascii_text_does_not_allocate() {
    let mut p = vt100::Parser::new(24, 80, 0);
    // a row of text then CR, so the stream never scrolls (scrolling allocates a fresh row)
    let line: Vec<u8> = b"proc 12345 root  42% text text text text text text text text\r".to_vec();
    p.process(&line); // warm up any lazily grown buffers
    let before = allocs();
    for _ in 0..2000 {
        p.process(&line);
    }
    assert_eq!(allocs() - before, 0, "printing allocated");
}

#[test]
fn printing_wide_and_combining_text_does_not_allocate_per_character() {
    let mut p = vt100::Parser::new(24, 80, 0);
    let line = "日本語 e\u{301}e\u{301} text\r".as_bytes().to_vec();
    p.process(&line);
    let before = allocs();
    for _ in 0..2000 {
        p.process(&line);
    }
    // 2000 lines x ~12 characters: any per-character allocation would blow far past this
    assert!(allocs() - before < 100, "allocated {}", allocs() - before);
}

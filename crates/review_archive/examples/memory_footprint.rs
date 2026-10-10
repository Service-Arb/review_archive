//! Anonymous RSS across a burst of capture encodes, then idle: what the pod keeps after its memory
//! lease ends, which kubelet will not shrink below.
//!
//! `cargo r --release -p review_archive --example memory_footprint -- <dir of PNGs>`

use std::path::{Path, PathBuf};

fn rss() -> String {
	let s = std::fs::read_to_string("/proc/self/status").expect("Linux");
	s.lines().filter(|l| l.starts_with("RssAnon") || l.starts_with("Threads")).collect::<Vec<_>>().join(" ")
}

fn pngs(dir: &Path, out: &mut Vec<PathBuf>) {
	for e in std::fs::read_dir(dir).unwrap() {
		let p = e.unwrap().path();
		match p.is_dir() {
			true => pngs(&p, out),
			false if p.extension().is_some_and(|e| e == "png") => out.push(p),
			false => {}
		}
	}
}

#[tokio::main]
async fn main() {
	let dir = std::env::args().nth(1).expect("usage: memory_footprint <dir of PNGs>");
	let mut files = Vec::new();
	pngs(Path::new(&dir), &mut files);
	println!("start ({} PNGs): {}", files.len(), rss());
	for (i, f) in files.iter().enumerate() {
		let c = review_archive::core::Capture {
			png: std::fs::read(f).unwrap(),
			captured_at: jiff::Timestamp::now(),
			page_url: "https://maps.google.com/?cid=1".into(),
		};
		review_archive::webp::provenance(&c, "footprint", "r").await.unwrap();
		if (i + 1) % 250 == 0 {
			println!("{} encoded: {}", i + 1, rss());
		}
	}
	tokio::time::sleep(std::time::Duration::from_secs(15)).await; // tokio's idle blocking threads exit after 10s
	println!("idle 15s: {}", rss());
	unsafe extern "C" {
		fn malloc_trim(pad: usize) -> i32;
	}
	// what is left after this is held live, not merely kept by glibc
	unsafe { malloc_trim(0) };
	println!("after malloc_trim: {}", rss());
}

//! The kit's class inventory and token sheet, written where `mfe.css` reads them: tailwind
//! can neither scan nor `@import` a crate unpacked from crates.io.

fn main() {
	println!("cargo:rerun-if-changed=build.rs");
	std::fs::write("uikit-classes.txt", ev_lib_classes::CLASS_INVENTORY).expect("the crate dir is writable");
	std::fs::create_dir_all("assets").expect("the crate dir is writable");
	std::fs::write("assets/tokens.css", ev_lib_classes::TOKENS_CSS).expect("the crate dir is writable");
}

fn main() {
    println!("cargo:rerun-if-changed=protocols/zwp-input-method-unstable-v2.xml");
    println!("cargo:rerun-if-changed=protocols/zwp-virtual-keyboard-unstable-v1.xml");
}

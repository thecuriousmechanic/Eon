fn main() {
    uniffi::generate_scaffolding("src/eon_core.udl").expect("invalid eon_core.udl");
}

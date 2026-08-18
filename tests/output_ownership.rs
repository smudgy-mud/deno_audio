#[test]
fn audio_backend_contract_is_not_publicly_injectable() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/output_backend_is_private.rs");
    cases.compile_fail("tests/ui/output_backend_module_is_private.rs");
}

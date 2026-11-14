//! veraPDF stand-ins on PATH. One test per file keeps the PATH change contained.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::*;
use pdf_squeezer::error::Ctx;
use pdf_squeezer::process::Out;
use pdf_squeezer::run::{Staged, Verifier};
use pdf_squeezer::tools::{declared_flavour, validate_pdfa};

fn sink() -> Out {
    Arc::new(Mutex::new(std::io::sink()))
}

fn set_path(dir: &Path) {
    // SAFETY: this test binary runs a single test, so nothing reads the environment concurrently.
    unsafe { std::env::set_var("PATH", dir) };
}

#[test]
fn verapdf_reports() {
    validation_fails_closed();
    verifier_separates_tool_failures_and_verdicts();
}

fn validation_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let document = dir.path().join("declared.pdf");
    write(&document, &pdfa_pdf("4", ""));
    let job = |report: &str| format!("<report><jobs><job><item><name>{}</name></item>{report}</job></jobs></report>", document.display());
    let failure = job(concat!(
        r#"<validationReport profileName="PDF/A-4 validation profile" isCompliant="false"><details>"#,
        r#"<rule specification="ISO 19005-4:2020" clause="6.2.10.4.1" testNumber="1" status="failed"><description>The font programs for all fonts used for rendering within a conforming file shall be embedded</description>"#,
        r#"<check status="failed"><context>root/document[0]/pages[0](3 0 obj PDPage)/contentStream[0](4 0 obj PDContentStream)/operators[3]/font[0](Helvetica)</context></check></rule>"#,
        r#"<rule specification="ISO 19005-4:2020" clause="6.1.3" testNumber="1" status="passed"><description>irrelevant</description></rule>"#,
        r#"</details></validationReport>"#
    ));
    let cases = [
        (job(r#"<validationReport profileName="PDF/A-4 validation profile" isCompliant="true"/>"#), ""),
        (failure.clone(), "fonts 6.2.10.4.1-1"),
        (job(r#"<validationReport profileName="PDF/A-2B validation profile" isCompliant="true"/>"#), "did not confirm"),
        ("<report/>".to_string(), "did not report"),
        ("broken xml".to_string(), "veraPDF report"),
        (String::new(), "no report"),
    ];
    set_path(dir.path());
    for (report, want) in cases {
        stub(&dir.path().join("verapdf"), &format!("#!/bin/sh\nprintf '%s' '{report}'\n"));
        let result = validate_pdfa(&Ctx::background(), "4", std::slice::from_ref(&document), &sink());
        let err = match result {
            Ok(mut verdicts) => verdicts.remove(&document).flatten(),
            Err(e) => Some(e),
        };
        match (want, &err) {
            ("", None) => {}
            ("", Some(e)) => panic!("compliant report rejected: {e}"),
            (_, None) => panic!("report {:.60} accepted, want {want:?}", report),
            (_, Some(e)) => assert!(e.to_string().contains(want), "report {:.60}: got {e}, want {want:?}", report),
        }
        if report == failure {
            assert!(err.unwrap().to_string().contains("object 4 Helvetica"), "diagnostics lack the object");
        }
    }
    let ctx = Ctx::background();
    for (part, conformance, want) in [("4", "", "4"), ("4", "F", "4f"), ("2", "B", "2b"), ("1", "A", "1a"), ("3", "U", "3u")] {
        write(&document, &pdfa_pdf(part, conformance));
        assert_eq!(declared_flavour(&ctx, &document).unwrap(), want, "part {part} conformance {conformance:?}");
    }
    write(&document, &pdfa_pdf("2", ""));
    assert!(declared_flavour(&ctx, &document).is_err(), "part 2 without conformance accepted");
    let plain = dir.path().join("plain.pdf");
    write(&plain, &test_pdf(0));
    assert_eq!(declared_flavour(&ctx, &plain).unwrap(), "");
}

fn verifier_separates_tool_failures_and_verdicts() {
    let dir = tempfile::tempdir().unwrap();
    stub(
        &dir.path().join("verapdf"),
        r#"#!/bin/sh
shift 6
[ "$#" = 1 ] || exit 1
case "$1" in
  */unavailable.pdf) exit 1 ;;
  */oversized.pdf) printf '%1048577s' ''; exit 0 ;;
  */malformed.pdf) printf 'not XML'; exit 0 ;;
  */rejected.pdf|*/invalid.pdf) compliant=false ;;
  *) compliant=true ;;
esac
printf '<report><jobs><job><item><name>%s</name></item><validationReport profileName="PDF/A-4 validation profile" isCompliant="%s"/></job></jobs></report>' "$1" "$compliant"
"#,
    );
    set_path(dir.path());
    let path = |name: &str| -> PathBuf { dir.path().join(format!("{name}.pdf")) };
    for (name, output, input, converted, want_error, tool_failure) in [
        ("tool failure", "unavailable", "valid", false, true, true),
        ("converted tool failure", "unavailable", "valid", true, true, true),
        ("oversized report", "oversized", "valid", false, true, true),
        ("malformed report", "malformed", "valid", false, true, true),
        ("compliant", "compliant", "valid", false, false, false),
        ("invalid input", "rejected", "invalid", false, false, false),
        ("valid input", "rejected", "valid", false, true, false),
        ("unverified input", "rejected", "unavailable", false, true, false),
        ("converted rejection", "rejected", "invalid", true, true, false),
    ] {
        let ctx = Ctx::background();
        let batch = vec![
            Staged::for_check(path(output), path("replaced"), path(input), "4", converted),
            Staged::for_check(path("other"), PathBuf::new(), PathBuf::new(), "4", false),
        ];
        let mut verifier = Verifier::new(sink());
        verifier.validate(&ctx, &batch);
        assert_eq!(verifier.tool_failed(0), tool_failure, "{name}: tool failure");
        let failed = verifier.check(&ctx, 0, &batch[0]);
        assert_eq!(failed.is_err(), want_error, "{name}: check error = {failed:?}");
        assert!(verifier.check(&ctx, 1, &batch[1]).is_ok(), "{name}: unrelated compliant file failed");
    }
}

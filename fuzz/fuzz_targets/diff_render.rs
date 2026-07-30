#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > 2 * 1024 * 1024 {
        return;
    }
    let midpoint = bytes.len() / 2;
    let (before, after) = bytes.split_at(midpoint);
    let (Ok(before), Ok(after)) = (std::str::from_utf8(before), std::str::from_utf8(after)) else {
        return;
    };
    let input = imara_diff::InternedInput::new(before, after);
    let mut diff = imara_diff::Diff::compute(imara_diff::Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let _ = diff
        .unified_diff(
            &imara_diff::BasicLineDiffPrinter(&input.interner),
            imara_diff::UnifiedDiffConfig::default(),
            &input,
        )
        .to_string();
});

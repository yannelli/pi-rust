//! Upstream Markdown regressions for wide display math and bare URLs next to CJK punctuation.

mod markdown_common;

use markdown_common::{Capabilities, markdown, plain, strip_ansi};
use pi_tui::{Component, visible_width};

#[test]
fn wide_display_math_falls_back_to_raw_source() {
    let _caps = Capabilities::plain();
    let source = [
        r"\[",
        r"\frac{\partial^2 u}{\partial x^2} + \frac{\partial^2 u}{\partial y^2} = \sum_{i=1}^{n} \frac{1}{i^2} + \int_0^1 f(x)\,dx",
        r"\]",
    ].join("\n");
    assert_eq!(
        plain(&markdown(&source).render(40)),
        ["∂² u   ∂² u   n   1   1", "──── + ──── = ∑   ── +∫ f(x) dx", "∂ x²   ∂ y²  i=1  i²  0",]
    );
    let narrow = plain(&markdown(&source).render(30));
    assert_eq!(narrow.first().map(String::as_str), Some(r"\["));
    assert_eq!(narrow.last().map(String::as_str), Some(r"\]"));
    assert!(narrow.iter().all(|line| !line.contains('─') && visible_width(line) <= 30));
}

fn linked(source: &str) -> String {
    markdown(source).render(160).join("")
}

#[test]
fn bare_urls_stop_at_cjk_punctuation_and_keep_the_following_text() {
    let _caps = Capabilities::set(true, false);
    let osc = regex::Regex::new(r"\x1b\]8;;[^\x1b]*\x1b\\").unwrap();
    for punctuation in "。，；：！？、“”‘’（）【】《》「」『』…—·・〈〉［］｛｝．｡｢｣､･".chars()
    {
        for url in ["https://example.com", "https://example.com/中文?q=搜索#片段", "www.example.com"] {
            let source = format!("{url}{punctuation}后面的中文");
            let output = linked(&source);
            let href = if url.starts_with("www.") { format!("http://{url}") } else { url.to_string() };
            assert!(output.contains(&format!("\x1b]8;;{href}\x1b\\")), "{source}");
            assert!(strip_ansi(&output).contains(&format!("\x1b]8;;\x1b\\{punctuation}后面的中文")), "{source}");
            assert_eq!(strip_ansi(&osc.replace_all(&output, "")).trim_end(), source);
        }
    }
}

#[test]
fn cjk_letters_remain_inside_bare_urls() {
    let _caps = Capabilities::set(true, false);
    for url in
        ["https://example.com紧接着中文", "https://example.com/中文?q=搜索#片段", "https://example.com/テストー한글"]
    {
        assert!(linked(url).contains(&format!("\x1b]8;;{url}\x1b\\")), "{url}");
    }
}

#[test]
fn explicit_links_and_angle_autolinks_keep_cjk_punctuation() {
    let _caps = Capabilities::set(true, false);
    let url = "https://example.com/中文，路径·テスト・한글…—。";
    for source in [format!("[文档]({url})"), format!("<{url}>")] {
        assert!(linked(&source).contains(&format!("\x1b]8;;{url}\x1b\\")), "{source}");
    }
}

#[test]
fn ascii_punctuation_is_trimmed_before_cjk_punctuation() {
    let _caps = Capabilities::set(true, false);
    let output = linked("https://example.com.，后面的中文");
    assert!(output.contains("\x1b]8;;https://example.com\x1b\\"));
    assert!(strip_ansi(&output).contains("\x1b]8;;\x1b\\.，后面的中文"));
}

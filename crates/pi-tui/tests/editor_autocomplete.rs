//! Ports of the upstream editor autocomplete tests: triggers, debouncing, and CJK boundaries.
//! Upstream ticks mock timers by 19 and 20 ms; here requests are checked before and after the
//! 20 ms debounce elapses on the real UI loop.

mod editor_common;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use editor_common::{
    MockProvider, TestEditor, before_cursor, cursor, editor, flush, flush_debounce, recorder, suggestions, type_text,
};
use pi_tui::Component;
use pi_tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider,
};

fn recording_provider(requests: &Rc<RefCell<Vec<String>>>, triggers: &[&str]) -> Rc<dyn AutocompleteProvider> {
    let requests = requests.clone();
    MockProvider::new(move |lines, line, col, _| {
        requests.borrow_mut().push(before_cursor(lines, line, col));
        None
    })
    .with_triggers(triggers)
    .shared()
}

/// The text typed, its editor, and the requests the provider saw.
type Case = (String, TestEditor, Rc<RefCell<Vec<String>>>);

/// The editor's symbol debounce. A check made after it elapsed cannot tell a debounced request from an early one.
const DEBOUNCE: Duration = Duration::from_millis(20);

const CJK_PUNCTUATION: &str = "\u{ff0c}\u{ff0e}\u{ff1a}\u{ff1b}\u{ff01}\u{ff1f}\u{ff08}\u{ff09}\u{ff3b}\u{ff3d}\u{ff5b}\u{ff5d}\u{201c}\u{201d}\u{2018}\u{2019}\u{2026}\u{2014}\u{3002}\u{3001}\u{300c}\u{300d}\u{300e}\u{300f}\u{300a}\u{300b}\u{3010}\u{3011}";

#[test]
fn triggers_and_debounces_symbols_after_cjk_punctuation() {
    let mut befores: Vec<String> = vec!["查看\u{ff0c}".into(), "\u{3000}".into()];
    befores.extend(CJK_PUNCTUATION.chars().map(String::from));
    befores.extend(["(", "see (", "`", "["].map(String::from));

    let mut combos: Vec<(String, &str)> = Vec::new();
    for before in &befores {
        for trigger in ["@", "#", "$", "-"] {
            combos.push((before.clone(), trigger));
        }
    }
    // Each case is checked right after its own input, while still inside the debounce.
    let mut cases: Vec<Case> = Vec::new();
    for (before, trigger) in combos {
        let editor = editor();
        let requests = recorder();
        editor.set_autocomplete_provider(recording_provider(&requests, &["$", "-"]));
        editor.set_text(&before);
        let typed = Instant::now();
        editor.handle_input(trigger);
        flush();
        let text = format!("{before}{trigger}");
        if typed.elapsed() < DEBOUNCE {
            assert!(requests.borrow().is_empty(), "requested before the debounce: {text:?}");
        }
        cases.push((text, editor, requests));
    }
    flush_debounce();
    for (text, editor, requests) in &cases {
        assert_eq!(*requests.borrow(), std::slice::from_ref(text));
        let typed = Instant::now();
        editor.handle_input("r");
        editor.handle_input("e");
        flush();
        if typed.elapsed() < DEBOUNCE {
            assert_eq!(requests.borrow().len(), 1, "{text:?}");
        }
    }
    flush_debounce();
    for (text, _, requests) in &cases {
        assert_eq!(*requests.borrow(), [text.clone(), format!("{text}re")]);
    }
}

#[test]
fn does_not_trigger_after_cjk_letters_or_for_unprefixed_paths() {
    let editor = editor();
    let calls = Rc::new(Cell::new(0));
    let counter = calls.clone();
    editor.set_autocomplete_provider(
        MockProvider::new(move |_, _, _, _| {
            counter.set(counter.get() + 1);
            None
        })
        .shared(),
    );
    for text in [
        "user@example.com",
        "张三@example.com",
        "查看@src",
        "あ@src",
        "カ@src",
        "한@src",
        "ㄅ@src",
        "𠮷@src",
        "か\u{3099}@src",
        "禰\u{e0100}@src",
        "々@src",
        "Ａ@src",
        "文档@备份",
        "prefix#123",
        "foo(@src",
        "问题#123",
        "查看\u{ff0c}/path/",
        "查看\u{ff0c}./文档/",
        "src/index.ts",
        "./文档/说明.md",
        "文档/说明.md",
        "查看src/index.ts",
    ] {
        editor.set_text("");
        type_text(&editor, text);
        flush_debounce();
        assert_eq!(calls.get(), 0, "{text:?}");
    }
}

#[test]
fn requests_path_completion_after_cjk_punctuation_only_on_tab() {
    let editor = editor();
    let requests: Rc<RefCell<Vec<(String, bool)>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = requests.clone();
    editor.set_autocomplete_provider(
        MockProvider::new(move |lines, line, col, options| {
            sink.borrow_mut().push((before_cursor(lines, line, col), options.force));
            None
        })
        .shared(),
    );
    let text = "查看\u{ff0c}/path/";
    type_text(&editor, text);
    flush_debounce();
    assert!(requests.borrow().is_empty());
    editor.handle_input("\t");
    flush();
    assert_eq!(*requests.borrow(), [(text.to_string(), true)]);
}

#[test]
fn completes_chinese_path_prefixes_with_tab() {
    let base = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir(base.path().join("文档")).expect("mkdir");
    std::fs::write(base.path().join("文档").join("说明.md"), "text").expect("write");
    let editor = editor();
    let base_path = base.path().to_string_lossy().into_owned();
    editor.set_autocomplete_provider(Rc::new(CombinedAutocompleteProvider::new(Vec::new(), base_path, None)));
    for separator in [" ", "\t", "\u{3000}", "\u{a0}", "\u{ff0c}", "\u{3002}"] {
        editor.set_text(&format!("查看{separator}"));
        let before = editor.get_text();
        editor.handle_input("文");
        editor.handle_input("\t");
        flush();
        assert_eq!(editor.get_text(), format!("{before}文档/"));
        editor.handle_input("说");
        editor.handle_input("\t");
        flush();
        assert_eq!(editor.get_text(), format!("{before}文档/说明.md"));
        assert_eq!(cursor(&editor), (0, editor.get_text().len()));
    }
}

#[test]
fn ends_trigger_and_debounce_contexts_at_separators() {
    for separator in [" ", "\u{3000}", "\u{ff0c}", "\u{3002}"] {
        for trigger in ["@", "#", "$"] {
            let editor = editor();
            let requests = recorder();
            let sink = requests.clone();
            let prefix = format!("{trigger}src");
            let expected_prefix = prefix.clone();
            editor.set_autocomplete_provider(
                MockProvider::new(move |lines, line, col, _| {
                    let text = before_cursor(lines, line, col);
                    sink.borrow_mut().push(text.clone());
                    (text == expected_prefix).then(|| AutocompleteSuggestions {
                        items: vec![pi_tui::AutocompleteItem::new(format!("{expected_prefix}/"), "src/")],
                        prefix: expected_prefix.clone(),
                    })
                })
                .with_triggers(&["$"])
                .shared(),
            );
            editor.set_text(&format!("{trigger}sr"));
            editor.handle_input("c");
            flush_debounce();
            assert!(editor.is_showing_autocomplete());
            editor.handle_input(separator);
            flush();
            assert_eq!(*requests.borrow(), [prefix.clone(), format!("{prefix}{separator}")]);
            assert!(!editor.is_showing_autocomplete());
            editor.handle_input("文");
            flush_debounce();
            assert_eq!(requests.borrow().len(), 2);
        }
    }
}

#[test]
fn retriggers_cjk_path_completion_after_directories_and_deleting() {
    let combined = Rc::new(CombinedAutocompleteProvider::new(Vec::new(), ".", None));
    for directory in ["文档", "我的 文档", "资料\u{ff0c}归档"] {
        let quoted = directory != "文档";
        let first_two: String = directory.chars().take(2).collect();
        let initial = if quoted { format!("@\"{first_two}") } else { "@文".to_string() };
        let directory_value = if quoted { format!("@\"{directory}/\"") } else { format!("@{directory}/") };
        let file_value = if quoted { format!("@\"{directory}/说明.md\"") } else { format!("@{directory}/说明.md") };
        let file_prefix = if quoted { format!("@\"{directory}/说") } else { format!("@{directory}/说") };

        let editor = editor();
        let (dir_value, file_value_inner, initial_inner) =
            (directory_value.clone(), file_value.clone(), initial.clone());
        let directory_label = format!("{directory}/");
        let apply = combined.clone();
        editor.set_autocomplete_provider(
            MockProvider::new(move |lines, line, col, _| {
                let before = before_cursor(lines, line, col);
                let prefix = before[before.find('@').unwrap_or(0)..].to_string();
                if prefix == initial_inner {
                    return Some(AutocompleteSuggestions {
                        items: vec![pi_tui::AutocompleteItem::new(dir_value.clone(), directory_label.clone())],
                        prefix,
                    });
                }
                (prefix == file_prefix).then(|| AutocompleteSuggestions {
                    items: vec![pi_tui::AutocompleteItem::new(file_value_inner.clone(), "说明.md")],
                    prefix,
                })
            })
            .with_apply(move |lines, line, col, item, prefix| apply.apply_completion(lines, line, col, item, prefix))
            .shared(),
        );
        editor.set_text(&format!("查看\u{ff1a}{initial}"));
        editor.handle_input("\t");
        flush();
        assert_eq!(editor.get_text(), format!("查看\u{ff1a}{directory_value}"));
        assert!(!editor.is_showing_autocomplete());

        editor.handle_input("说");
        flush_debounce();
        assert!(editor.is_showing_autocomplete());

        for deletion in ["\x7f", "\x1b[3~"] {
            editor.handle_input("错");
            flush_debounce();
            assert!(!editor.is_showing_autocomplete());
            if deletion == "\x1b[3~" {
                editor.handle_input("\x1b[D");
            }
            editor.handle_input(deletion);
            flush_debounce();
            assert!(editor.is_showing_autocomplete(), "{directory} after {deletion:?}");
        }

        editor.handle_input("\t");
        assert_eq!(editor.get_text(), format!("查看\u{ff1a}{file_value} "));
        assert_eq!(cursor(&editor), (0, editor.get_text().len()));
    }
}

#[test]
fn auto_applies_a_single_forced_suggestion() {
    let editor = editor();
    editor.set_autocomplete_provider(
        MockProvider::new(|lines, _, col, options| {
            (options.force && &lines[0][..col] == "Work").then(|| suggestions(&["Workspace/"], "Work")).flatten()
        })
        .shared(),
    );
    type_text(&editor, "Work");
    editor.handle_input("\t");
    flush();
    assert_eq!(editor.get_text(), "Workspace/");
    assert!(!editor.is_showing_autocomplete());
    editor.handle_input(editor_common::UNDO);
    assert_eq!(editor.get_text(), "Work");
}

#[test]
fn shows_the_menu_for_several_forced_suggestions() {
    let editor = editor();
    editor.set_autocomplete_provider(
        MockProvider::new(|lines, _, col, options| {
            (options.force && &lines[0][..col] == "src").then(|| suggestions(&["src/", "src.txt"], "src")).flatten()
        })
        .shared(),
    );
    type_text(&editor, "src");
    editor.handle_input("\t");
    flush();
    assert_eq!(editor.get_text(), "src");
    assert!(editor.is_showing_autocomplete());
    editor.handle_input("\t");
    assert_eq!(editor.get_text(), "src/");
    assert!(!editor.is_showing_autocomplete());
}

#[test]
fn keeps_suggestions_open_while_typing_in_force_mode() {
    let editor = editor();
    let all = ["readme.md", "package.json", "src/", "dist/"];
    editor.set_autocomplete_provider(
        MockProvider::new(move |lines, _, col, options| {
            let prefix = &lines[0][..col];
            if !(options.force || prefix.contains('/') || prefix.starts_with('.')) {
                return None;
            }
            let filtered: Vec<&str> =
                all.iter().copied().filter(|f| f.to_lowercase().starts_with(&prefix.to_lowercase())).collect();
            (!filtered.is_empty()).then(|| suggestions(&filtered, prefix)).flatten()
        })
        .shared(),
    );
    editor.handle_input("\t");
    flush();
    assert!(editor.is_showing_autocomplete());
    for (key, expected) in [("r", "r"), ("e", "re")] {
        editor.handle_input(key);
        flush();
        assert_eq!(editor.get_text(), expected);
        assert!(editor.is_showing_autocomplete());
    }
    editor.handle_input("\t");
    assert_eq!(editor.get_text(), "readme.md");
    assert!(!editor.is_showing_autocomplete());
}

fn debounced_symbol(trigger_text: &str, value: &'static str, triggers: &[&str]) {
    let editor = editor();
    let calls = Rc::new(Cell::new(0));
    let counter = calls.clone();
    editor.set_autocomplete_provider(
        MockProvider::new(move |lines, line, col, _| {
            counter.set(counter.get() + 1);
            suggestions(&[value], &before_cursor(lines, line, col))
        })
        .with_triggers(triggers)
        .shared(),
    );
    type_text(&editor, trigger_text);
    flush();
    assert_eq!(calls.get(), 0);
    assert!(!editor.is_showing_autocomplete());
    flush_debounce();
    assert_eq!(calls.get(), 1);
    assert!(editor.is_showing_autocomplete());
}

#[test]
fn debounces_at_hash_and_custom_triggers() {
    debounced_symbol("@mai", "@main.ts", &[]);
    debounced_symbol("#298", "#2983", &[]);
    debounced_symbol("$sk", "$skill-name", &["$"]);
}

#[test]
fn resets_custom_triggers_when_the_provider_changes() {
    let editor = editor();
    editor.set_autocomplete_provider(
        MockProvider::new(|_, _, _, _| suggestions(&["$skill-name"], "$")).with_triggers(&["$"]).shared(),
    );
    let calls = Rc::new(Cell::new(0));
    let counter = calls.clone();
    editor.set_autocomplete_provider(
        MockProvider::new(move |_, _, _, _| {
            counter.set(counter.get() + 1);
            suggestions(&["$skill-name"], "$")
        })
        .shared(),
    );
    type_text(&editor, "$s");
    flush_debounce();
    assert_eq!(calls.get(), 0);
    assert!(!editor.is_showing_autocomplete());
}

/// A provider that offers `/command-0` to `/command-11` for `/`.
fn twelve_commands() -> Rc<dyn AutocompleteProvider> {
    MockProvider::new(|_, _, _, _| {
        let items = (0..12).map(|i| AutocompleteItem::new(format!("/command-{i}"), format!("command-{i}"))).collect();
        Some(AutocompleteSuggestions { items, prefix: "/".into() })
    })
    .shared()
}

#[test]
fn moves_the_autocomplete_selection_with_selection_page_keys() {
    let editor = editor();
    let submitted = recorder();
    let sink = submitted.clone();
    editor.on_submit(move |text| sink.borrow_mut().push(text.to_string()));
    editor.set_autocomplete_provider(twelve_commands());

    assert!(!editor.captures_viewport_input());
    editor.handle_input("/");
    flush();
    assert!(editor.captures_viewport_input());

    editor.handle_input("\x1b[6~");
    editor.handle_input("\x1b[6~");
    editor.handle_input("\x1b[5~");
    editor.handle_input("\r");

    assert_eq!(*submitted.borrow(), ["/command-5"]);
}

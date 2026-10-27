//! Device-code login actions and URL handling of the login dialog; ports
//! `login-dialog-device-code.test.ts`.

use std::rc::Rc;

use pi_ai::auth::AuthInfoLink;
use pi_tui::Component;

use super::login_dialog::dialog_harness;
use crate::interactive_components::login::{ClipboardCopyFn, DeviceCodeInfo, LoginDialog};
use crate::interactive_components::test_support::{Calls, Harness, flush, keys, local};

const CODE: &str = "ABCD-EFGH";
const PAGE: &str = "https://example.com/device";

fn device(open_browser_on_confirm: Option<bool>, copy_code_on_confirm: Option<bool>) -> DeviceCodeInfo {
    DeviceCodeInfo {
        user_code: CODE.into(),
        verification_uri: PAGE.into(),
        open_browser_on_confirm,
        copy_code_on_confirm,
    }
}

// Regression upstream #9282: device-code convenience actions require explicit confirmation.
#[tokio::test(flavor = "current_thread")]
async fn opens_the_verification_page_and_copies_the_code_once_after_confirmation() {
    local(async {
        let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
        let harness = dialog_harness(&copied, &opened, &completed);
        let domain = harness.component.borrow_mut().show_prompt("GitHub Enterprise URL/domain", None);
        harness.send(keys::ENTER);
        assert_eq!(domain.await.unwrap(), "");
        harness.component.borrow_mut().show_device_code(&device(None, None));
        harness.component.borrow_mut().show_waiting("Waiting for authentication...");
        harness.settle();
        assert!(harness.screen().contains("enter to open browser and copy code"), "{}", harness.screen());
        assert!(opened.is_empty() && copied.is_empty());

        harness.send(keys::ENTER);
        flush(&harness).await;
        harness.send(keys::ENTER);
        assert_eq!(opened.get(), [PAGE]);
        assert_eq!(copied.get(), [CODE]);
        let screen = harness.screen();
        assert!(screen.contains("Code copied to clipboard"), "{screen}");
        assert!(screen.contains(&format!("Enter code: {CODE}")));
        assert!(screen.contains("Waiting for authentication..."));
        assert!(!screen.contains("to open browser and copy code"));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn honors_provider_overrides_for_browser_and_clipboard() {
    for (open_browser, copy_code) in [(false, false), (true, false), (false, true)] {
        local(async {
            let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
            let harness = dialog_harness(&copied, &opened, &completed);
            harness.component.borrow_mut().show_device_code(&device(Some(open_browser), Some(copy_code)));
            harness.send(keys::ENTER);
            flush(&harness).await;
            assert_eq!(opened.len(), usize::from(open_browser), "{open_browser} {copy_code}");
            assert_eq!(copied.len(), usize::from(copy_code), "{open_browser} {copy_code}");
            assert!(!harness.component.borrow().cancel_token().is_cancelled());
        })
        .await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn switching_steps_clears_the_previous_copy_and_confirm_actions() {
    local(async {
        let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
        let harness = dialog_harness(&copied, &opened, &completed);
        harness.component.borrow_mut().show_auth("https://example.com/authorize", None);
        harness.component.borrow_mut().show_device_code(&device(None, None));
        opened.take();
        harness.send(&keys::ctrl('x'));
        assert!(copied.is_empty() && opened.is_empty());
        harness.send(keys::ENTER);
        assert_eq!(copied.take(), [CODE]);

        harness.component.borrow_mut().show_device_code(&device(None, None));
        harness.component.borrow_mut().show_auth("https://example.com/authorize", None);
        opened.take();
        harness.send(keys::ENTER);
        assert!(copied.is_empty() && opened.is_empty());
        harness.send(&keys::ctrl('x'));
        assert_eq!(copied.take(), ["https://example.com/authorize"]);

        harness.component.borrow_mut().show_details(&["Choose a login method".to_string()]);
        harness.send(&keys::ctrl('x'));
        harness.component.borrow_mut().show_device_code(&device(None, None));
        harness.component.borrow_mut().show_details(&["Choose a login method".to_string()]);
        opened.take();
        harness.send(keys::ENTER);
        assert!(copied.is_empty() && opened.is_empty());
        flush(&harness).await;
    })
    .await;
}

#[test]
fn cancel_neither_opens_a_browser_nor_changes_the_clipboard() {
    let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
    let harness = dialog_harness(&copied, &opened, &completed);
    harness.component.borrow_mut().show_device_code(&device(None, None));
    harness.send(keys::ESCAPE);
    harness.send(keys::ENTER);
    assert!(harness.component.borrow().cancel_token().is_cancelled());
    assert_eq!(completed.get(), [(false, Some("Login cancelled".to_string()))]);
    assert!(copied.is_empty() && opened.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn keeps_manual_login_available_when_the_clipboard_fails() {
    local(async {
        let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
        let harness = dialog_harness(&copied, &opened, &completed);
        let failing: ClipboardCopyFn = Rc::new(|_| Box::pin(async { Err("Clipboard unavailable".to_string()) }));
        harness.component.borrow_mut().set_clipboard(failing);
        harness.component.borrow_mut().show_device_code(&device(None, None));
        harness.component.borrow_mut().show_waiting("Waiting for authentication...");
        harness.send(keys::ENTER);
        flush(&harness).await;
        assert_eq!(opened.get(), [PAGE]);
        let screen = harness.screen();
        assert!(screen.contains(PAGE) && screen.contains(&format!("Enter code: {CODE}")), "{screen}");
        assert!(screen.contains("Copy code manually"));
        assert!(screen.contains("Waiting for authentication..."));
        assert!(!harness.component.borrow().cancel_token().is_cancelled());
        assert!(completed.is_empty());
    })
    .await;
}

#[test]
fn opens_only_http_sign_in_urls() {
    let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
    let harness = dialog_harness(&copied, &opened, &completed);
    harness.component.borrow_mut().show_auth("file:///tmp/payload.app", None);
    assert!(opened.is_empty());
    harness.component.borrow_mut().show_auth("https://example.com/authorize", None);
    assert_eq!(opened.get(), ["https://example.com/authorize"]);
}

fn raw_render(harness: &Harness<LoginDialog>) -> String {
    harness.component.borrow_mut().render(120).join("\n")
}

#[test]
fn control_characters_in_urls_cannot_end_terminal_hyperlinks() {
    let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
    let harness = dialog_harness(&copied, &opened, &completed);
    let info = DeviceCodeInfo { verification_uri: format!("{PAGE}\x07\x1b]52;c;Zm9v\x07"), ..device(None, None) };
    let help =
        AuthInfoLink { url: "https://example.com/help\x07\x1b]52;c;Zm9v\x07".into(), label: Some("Help".into()) };
    harness.component.borrow_mut().show_device_code(&info);
    harness.component.borrow_mut().show_info("Docs", &[help], false);
    assert!(!raw_render(&harness).contains("\x1b]52"));
    let shown = harness.render(120).join("\n");
    assert!(shown.contains("https://example.com/device%07%1B]52;c;Zm9v"), "{shown}");
    assert!(shown.contains("Help: https://example.com/help%07%1B]52;c;Zm9v"), "{shown}");
}

#[test]
fn control_characters_in_other_urls_are_replaced() {
    let injected = "x\x07\x1b]52;c;Zm9v\x07";
    let (copied, opened, completed) = (Calls::new(), Calls::new(), Calls::new());
    let harness = dialog_harness(&copied, &opened, &completed);
    harness.component.borrow_mut().show_auth(&format!("file:///tmp/{injected}"), None);
    assert!(!raw_render(&harness).contains("\x1b]52"));

    let info = DeviceCodeInfo { verification_uri: injected.into(), ..device(None, None) };
    harness.component.borrow_mut().show_device_code(&info);
    assert!(!raw_render(&harness).contains("\x1b]52"));
    assert!(harness.render(120).join("\n").contains("x ]52;c;Zm9v"));

    let help = AuthInfoLink { url: injected.into(), label: Some("Help".into()) };
    harness.component.borrow_mut().show_info("Docs", &[help], false);
    assert!(!raw_render(&harness).contains("\x1b]52"));
    assert!(harness.render(120).join("\n").contains("Help: x ]52;c;Zm9v"));
    assert!(opened.is_empty());
}

//! Loopback callback server pages. Ported from packages/mcp/test/oauth.test.ts.

use std::sync::{Arc, Mutex};

use pi_mcp::oauth::{CallbackPage, CallbackServer, CallbackServerOptions, LoopbackCallbackServer};
use url::Url;

#[tokio::test]
async fn renders_plain_text_by_default() {
    let callback = LoopbackCallbackServer::listen(CallbackServerOptions::default()).await.unwrap();
    let pending = callback.wait_for_callback("s1", None).unwrap();
    let response = reqwest::get(format!("{}?code=abc&state=s1", callback.redirect_url())).await.unwrap();
    assert_eq!(response.headers()["content-type"], "text/plain; charset=utf-8");
    assert_eq!(response.text().await.unwrap(), "Authorization complete. You may close this window.");
    assert_eq!(pending.await.unwrap().code, "abc");
    callback.close().await;
}

#[tokio::test]
async fn rejects_a_response_on_another_path_than_the_expected_one() {
    let options = CallbackServerOptions { extra_paths: vec!["/callback/server-id".into()], ..Default::default() };
    let callback = LoopbackCallbackServer::listen(options).await.unwrap();
    let origin = Url::parse(&callback.redirect_url()).unwrap().origin().ascii_serialization();
    let mixed_up = callback.wait_for_callback("s1", Some("/callback/server-id")).unwrap();
    let wrong = reqwest::get(format!("{origin}/callback?code=abc&state=s1")).await.unwrap();
    assert_eq!(wrong.status(), 400);
    let error = mixed_up.await.unwrap_err().to_string();
    assert!(error.contains("arrived on another redirect URI"), "{error}");

    let pending = callback.wait_for_callback("s2", Some("/callback/server-id")).unwrap();
    let right = reqwest::get(format!("{origin}/callback/server-id?code=abc&state=s2")).await.unwrap();
    assert_eq!(right.status(), 200);
    assert_eq!(pending.await.unwrap().code, "abc");
    callback.close().await;
}

#[tokio::test]
async fn renders_pages_through_render_page() {
    let pages = Arc::new(Mutex::new(Vec::new()));
    let seen = pages.clone();
    let render = Arc::new(move |page: &CallbackPage| {
        seen.lock().unwrap().push(page.clone());
        match page {
            CallbackPage::Ok => "<p>ok</p>".to_string(),
            CallbackPage::Failed { message, .. } => format!("<p>{message}</p>"),
        }
    });
    let options = CallbackServerOptions { render_page: Some(render), ..Default::default() };
    let callback = LoopbackCallbackServer::listen(options).await.unwrap();
    let denied = callback.wait_for_callback("s1", None).unwrap();
    let url = format!("{}?error=access_denied&error_description=Denied&state=s1", callback.redirect_url());
    let failure = reqwest::get(url).await.unwrap();
    assert_eq!(failure.headers()["content-type"], "text/html; charset=utf-8");
    let error = denied.await.unwrap_err().to_string();
    assert!(error.contains("Denied"), "{error}");
    let failed = CallbackPage::Failed {
        message: "Authorization failed. You may close this window.".into(),
        details: Some("Denied".into()),
    };
    assert_eq!(pages.lock().unwrap().last(), Some(&failed));

    let pending = callback.wait_for_callback("s2", None).unwrap();
    let success = reqwest::get(format!("{}?code=abc&state=s2", callback.redirect_url())).await.unwrap();
    assert_eq!(success.text().await.unwrap(), "<p>ok</p>");
    assert_eq!(pending.await.unwrap().code, "abc");
    callback.close().await;
}

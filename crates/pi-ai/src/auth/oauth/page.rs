//! The HTML page a loopback OAuth callback shows in the browser.

const LOGO_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 800" aria-hidden="true"><path fill="#F09082" d="M165.29 165.29H517.36V400H400V282.65H165.29Z"/><path fill="#4D9ABF" d="M165.29 282.65H282.65V400H400V517.36H282.65V634.72H165.29Z"/><path fill="#F1BE58" d="M517.36 400H634.72V634.72H517.36Z"/></svg>"##;

const STYLE: &str = r#"    :root {
      --text: #fafafa;
      --text-dim: #a1a1aa;
      --page-bg: #09090b;
      --font-sans: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, "Noto Sans", sans-serif, "Apple Color Emoji", "Segoe UI Emoji", "Segoe UI Symbol", "Noto Color Emoji";
      --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
    }
    * { box-sizing: border-box; }
    html { color-scheme: dark; }
    body {
      margin: 0;
      min-height: 100vh;
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 24px;
      background: var(--page-bg);
      color: var(--text);
      font-family: var(--font-sans);
      text-align: center;
    }
    main {
      width: 100%;
      max-width: 560px;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
    }
    .logo {
      width: 72px;
      height: 72px;
      display: block;
      margin-bottom: 24px;
    }
    h1 {
      margin: 0 0 10px;
      font-size: 28px;
      line-height: 1.15;
      font-weight: 650;
      color: var(--text);
    }
    p {
      margin: 0;
      line-height: 1.7;
      color: var(--text-dim);
      font-size: 15px;
    }
    .details {
      margin-top: 16px;
      font-family: var(--font-mono);
      font-size: 13px;
      color: var(--text-dim);
      white-space: pre-wrap;
      word-break: break-word;
    }"#;

fn escape_html(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn render_page(title: &str, heading: &str, message: &str, details: Option<&str>) -> String {
    let title = escape_html(title);
    let heading = escape_html(heading);
    let message = escape_html(message);
    let details = match details.filter(|details| !details.is_empty()) {
        Some(details) => format!(r#"<div class="details">{}</div>"#, escape_html(details)),
        None => String::new(),
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{title}</title>
  <style>
{STYLE}
  </style>
</head>
<body>
  <main>
    <div class="logo">{LOGO_SVG}</div>
    <h1>{heading}</h1>
    <p>{message}</p>
    {details}
  </main>
</body>
</html>"#
    )
}

/// The page shown after a successful sign-in.
pub fn oauth_success_html(message: &str) -> String {
    render_page("Authentication successful", "Authentication successful", message, None)
}

/// The page shown after a failed sign-in, with optional technical details.
pub fn oauth_error_html(message: &str, details: Option<&str>) -> String {
    render_page("Authentication failed", "Authentication failed", message, details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_escape_text_and_include_the_logo() {
        let success = oauth_success_html("Signed in to <Example>.");
        assert!(success.contains("<title>Authentication successful</title>"));
        assert!(success.contains("Signed in to &lt;Example&gt;."));
        assert!(success.contains(r##"fill="#F09082""##));
        assert!(!success.contains(r#"class="details""#));
        let failure = oauth_error_html("Failed.", Some("it's \"bad\" & worse"));
        assert!(failure.contains("<h1>Authentication failed</h1>"));
        assert!(failure.contains(r#"<div class="details">it&#39;s &quot;bad&quot; &amp; worse</div>"#));
    }
}

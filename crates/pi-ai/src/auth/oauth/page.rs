//! The HTML page a loopback OAuth callback shows in the browser.

const LOGO_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 800" aria-hidden="true"><defs><linearGradient id="oxide" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#E0712F"/><stop offset="0.55" stop-color="#B7471A"/><stop offset="1" stop-color="#8A3212"/></linearGradient></defs><path fill="url(#oxide)" d="M351.81 77.58L363.20 17.77A384 384 0 0 1 436.80 17.77L448.19 77.58A326 326 0 0 1 519.48 96.68L559.24 50.57A384 384 0 0 1 622.99 87.38L602.94 144.87A326 326 0 0 1 655.13 197.06L712.62 177.01A384 384 0 0 1 749.43 240.76L703.32 280.52A326 326 0 0 1 722.42 351.81L782.23 363.20A384 384 0 0 1 782.23 436.80L722.42 448.19A326 326 0 0 1 703.32 519.48L749.43 559.24A384 384 0 0 1 712.62 622.99L655.13 602.94A326 326 0 0 1 602.94 655.13L622.99 712.62A384 384 0 0 1 559.24 749.43L519.48 703.32A326 326 0 0 1 448.19 722.42L436.80 782.23A384 384 0 0 1 363.20 782.23L351.81 722.42A326 326 0 0 1 280.52 703.32L240.76 749.43A384 384 0 0 1 177.01 712.62L197.06 655.13A326 326 0 0 1 144.87 602.94L87.38 622.99A384 384 0 0 1 50.57 559.24L96.68 519.48A326 326 0 0 1 77.58 448.19L17.77 436.80A384 384 0 0 1 17.77 363.20L77.58 351.81A326 326 0 0 1 96.68 280.52L50.57 240.76A384 384 0 0 1 87.38 177.01L144.87 197.06A326 326 0 0 1 197.06 144.87L177.01 87.38A384 384 0 0 1 240.76 50.57L280.52 96.68A326 326 0 0 1 351.81 77.58Z"/><circle cx="400" cy="400" r="292" fill="#231A16"/><circle cx="400" cy="400" r="278" fill="none" stroke="#3A2A22" stroke-width="4"/><path fill="#EF6F3A" d="M224 224H488V400H400V312H224Z"/><path fill="#5FA0BF" d="M224 312H312V400H400V488H312V576H224Z"/><path fill="#F2B44C" d="M488 400H576V576H488Z"/></svg>"##;

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
        assert!(success.contains(r##"fill="#EF6F3A""##));
        assert!(!success.contains(r#"class="details""#));
        let failure = oauth_error_html("Failed.", Some("it's \"bad\" & worse"));
        assert!(failure.contains("<h1>Authentication failed</h1>"));
        assert!(failure.contains(r#"<div class="details">it&#39;s &quot;bad&quot; &amp; worse</div>"#));
    }
}

//! Vectors from the AWS SigV4 test suite (`aws-sig-v4-test-suite`) and the IAM signing
//! example in the AWS documentation, all signed at 2015-08-30T12:36:00Z.

use super::*;

const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

fn test_time() -> SystemTime {
    parse_rfc3339("2015-08-30T12:36:00Z").unwrap()
}

fn authorization(method: &str, url: &str, headers: &[(&str, &str)], body: &str, service: &str) -> String {
    let url = reqwest::Url::parse(url).unwrap();
    let headers: Vec<(String, String)> = headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
    let request = SignableRequest { method, url: &url, headers: &headers, body: body.as_bytes() };
    let signed = sign(&request, &Credentials::new("AKIDEXAMPLE", SECRET), "us-east-1", service, test_time());
    signed.into_iter().find(|(name, _)| name == "authorization").unwrap().1
}

fn signature(authorization: &str) -> &str {
    authorization.rsplit("Signature=").next().unwrap()
}

#[test]
fn signing_key_matches_the_documented_derivation() {
    let key = signing_key(SECRET, "20150830", "us-east-1", "iam");
    assert_eq!(hex(&key), "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9");
}

#[test]
fn test_suite_get_and_post_vanilla() {
    let get = authorization("GET", "https://example.amazonaws.com/", &[], "", "service");
    assert_eq!(
        get,
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
    );
    let post = authorization("POST", "https://example.amazonaws.com/", &[], "", "service");
    assert_eq!(signature(&post), "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b");
}

#[test]
fn test_suite_query_order_and_form_body() {
    let query = authorization("GET", "https://example.amazonaws.com/?Param2=value2&Param1=value1", &[], "", "service");
    assert_eq!(signature(&query), "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500");
    let form = authorization(
        "POST",
        "https://example.amazonaws.com/",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        "Param1=value1",
        "service",
    );
    assert!(form.contains("SignedHeaders=content-type;host;x-amz-date"), "{form}");
    assert_eq!(signature(&form), "ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a");
}

#[test]
fn documented_iam_list_users_example() {
    let iam = authorization(
        "GET",
        "https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08",
        &[("content-type", "application/x-www-form-urlencoded; charset=utf-8")],
        "",
        "iam",
    );
    assert_eq!(signature(&iam), "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7");
}

#[test]
fn session_tokens_are_signed_and_bedrock_labels_double_encoded() {
    let url = reqwest::Url::parse("https://example.amazonaws.com/").unwrap();
    let credentials =
        Credentials { session_token: Some("SESSIONTOKEN".into()), ..Credentials::new("AKIDEXAMPLE", SECRET) };
    let request = SignableRequest { method: "POST", url: &url, headers: &[], body: b"" };
    let signed = sign(&request, &credentials, "us-east-1", "service", test_time());
    assert_eq!(signed[2], ("x-amz-security-token".to_string(), "SESSIONTOKEN".to_string()));
    assert!(signed[0].1.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"));
    assert_eq!(signature(&signed[0].1), "0552c3dc6731a2ff8a351d57231cae7149e10d2c922a368491cfce1d1f28564e");

    let url = "https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse-stream";
    let url = reqwest::Url::parse(url).unwrap();
    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    let request = SignableRequest { method: "POST", url: &url, headers: &headers, body: b"{\"messages\":[]}" };
    let signed = sign(&request, &Credentials::new("AKIDEXAMPLE", SECRET), "us-west-2", "bedrock", test_time());
    assert_eq!(signature(&signed[0].1), "7a2f1c5799d0eea61d2831ad5d2dae8a46294e4ecaa465a04e25288cf85a981a");
}

#[test]
fn formats_and_parses_utc_timestamps() {
    assert_eq!(format_amz_date(test_time()), "20150830T123600Z");
    assert_eq!(parse_rfc3339("2015-08-30T14:36:00.123+02:00"), Some(test_time()));
    assert_eq!(format_amz_date(parse_rfc3339("2024-02-29T23:59:59Z").unwrap()), "20240229T235959Z");
    assert_eq!(parse_rfc3339("not a date"), None);
}

#[test]
fn formats_iso8601_like_javascript() {
    let time = parse_rfc3339("2024-02-29T23:59:58Z").unwrap() + Duration::from_millis(7);
    assert_eq!(format_iso8601(time), "2024-02-29T23:59:58.007Z");
}

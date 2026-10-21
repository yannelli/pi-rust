//! Port of upstream `test/contract.test.ts`, plus contract descriptions, fingerprints, coverage and binding checked
//! against golden fixtures from upstream TypeScript.

mod chord2_support;

use std::collections::HashMap;

use chord2_support::*;
use pi_chord::host2::schema::{canonical_identity, canonical_json, fits, normalize};
use pi_chord::host2::{
    ChordErrorCode, Contract, Description, LocalDef, Member, MemberDescription, RemoteDef, VariantDef, bind,
    contract_from_description, coverage, host_family, host_named, is_chord_error, local, method, opaque, remote,
    takes_no_args,
};
use pi_chord::{Json, json};

fn view() -> Json {
    object(&[req("text", string())])
}

fn chat_def() -> RemoteDef {
    RemoteDef {
        version: 1,
        input: object(&[
            req("session", string()),
            req("conversation", with(number(), serde_json::json!({"default": 0}))),
        ]),
        host: host_family("session:", "session").unwrap(),
        variants: vec![
            ("viewer".into(), VariantDef::new().state("view", view())),
            (
                "participant".into(),
                VariantDef::new()
                    .state("view", view())
                    .method("send", method(object(&[req("text", string())]), object(&[req("id", number())]))),
            ),
        ],
    }
}

fn chat_v1() -> Contract {
    remote("t.chat", &chat_def()).unwrap()
}

/// `chatV1` with another `participant` variant.
fn with_participant(view: Json, send: Member) -> Contract {
    let mut def = chat_def();
    def.variants[1].1 = VariantDef::new().state("view", view).method("send", send);
    remote("t.chat", &def).unwrap()
}

#[test]
fn normalizes_input_and_omits_defaults_from_the_identity() {
    let chat = chat_v1();
    let a = normalize(chat.input(), Some(&json!({"session": "s", "extra": 1})), "input").unwrap();
    assert_eq!(a, json!({"session": "s", "conversation": 0}));
    let b = normalize(chat.input(), Some(&json!({"conversation": 0, "session": "s"})), "input").unwrap();
    assert_eq!(canonical_identity(chat.input(), &a), canonical_identity(chat.input(), &b));
    assert_eq!(canonical_identity(chat.input(), &a), r#"{"session":"s"}"#);
    let error = normalize(chat.input(), Some(&json!({"session": 1})), "input").unwrap_err();
    assert!(is_chord_error(&error, Some(ChordErrorCode::Invalid)) && error.reason() == Some("input"));
}

#[test]
fn fingerprints_are_stable_and_description_based() {
    let copy = remote("t.chat", &chat_def()).unwrap();
    assert_eq!(copy.fingerprint(), chat_v1().fingerprint());
    let v2 = remote("t.chat", &RemoteDef { version: 2, ..chat_def() }).unwrap();
    assert_ne!(v2.fingerprint(), chat_v1().fingerprint());
}

#[test]
fn rejects_reserved_member_names_and_opaque_remote_members() {
    let reserved =
        LocalDef { version: 1, input: object(&[]), members: vec![("status".into(), method(None, None).into())] };
    assert!(local("t.x", &reserved).is_err());
    let opaque_state = RemoteDef {
        host: host_named("server").unwrap(),
        version: 1,
        input: object(&[]),
        variants: vec![("a".into(), VariantDef::new().state("h", opaque("x@1").unwrap()))],
    };
    let error = remote("t.x", &opaque_state).unwrap_err();
    assert_eq!(error.to_string(), "remote contracts cannot expose opaque members: h");
}

#[test]
fn covers_additive_changes_and_refuses_breaking_ones() {
    let chat = chat_v1();
    let additive = with_participant(
        object(&[req("text", string()), req("extra", number())]),
        method(
            object(&[req("text", string()), opt("urgent", boolean())]),
            object(&[req("id", number()), req("at", number())]),
        )
        .into(),
    );
    assert_eq!(coverage(additive.description(), chat.description()), None);
    assert!(bind::<()>(&additive, None, &chat).unwrap().is_none());

    let required = with_participant(
        view(),
        method(object(&[req("text", string()), req("urgent", boolean())]), object(&[req("id", number())])).into(),
    );
    assert!(coverage(required.description(), chat.description()).unwrap().contains("arguments"));
    let error = bind::<()>(&required, None, &chat).unwrap_err();
    assert!(is_chord_error(&error, Some(ChordErrorCode::ContractMismatch)));

    let mut def = chat_def();
    def.variants[0].1 = VariantDef::new().state("view", object(&[req("text", number())]));
    let retyped = remote("t.chat", &def).unwrap();
    assert!(coverage(retyped.description(), chat.description()).unwrap().contains("state"));
}

#[test]
fn methods_without_arguments_or_result_are_described_without_them() {
    let of = |member: Member| {
        let def = LocalDef { version: 1, input: object(&[]), members: vec![("m".into(), member)] };
        local("t.methods", &def).unwrap().description().clone()
    };
    let none = of(method(None, None).into());
    assert_eq!(none.to_json()["variants"][""]["m"], json!({"kind": "method"}));
    let with_args = of(method(string(), None).into());
    let with_result = of(method(None, number()).into());
    assert_eq!(coverage(&none, &none), None);
    // The consumer sends none, the provider expects some; and the other way around.
    assert!(coverage(&with_args, &none).unwrap().contains("arguments"));
    assert!(coverage(&none, &with_args).unwrap().contains("arguments"));
    // A void consumer ignores a result; a consumer expecting a result needs one.
    assert_eq!(coverage(&with_result, &none), None);
    assert!(coverage(&none, &with_result).unwrap().contains("result"));
}

#[test]
fn binds_other_versions_only_through_accepts_adapters() {
    let chat = chat_v1();
    let v2 = remote(
        "t.chat",
        &RemoteDef {
            version: 2,
            variants: vec![("viewer".into(), VariantDef::new().state("body", view()))],
            ..chat_def()
        },
    )
    .unwrap();
    let error = bind::<&str>(&v2, None, &chat).unwrap_err();
    assert!(is_chord_error(&error, Some(ChordErrorCode::ContractMismatch)));
    let accepts = HashMap::from([(1, "adapter")]);
    assert_eq!(bind(&v2, Some(&accepts), &chat).unwrap(), Some(&"adapter"));
}

#[test]
fn compares_opaque_members_by_id_and_function_members_structurally() {
    let members = |harness: &str| {
        vec![
            ("harness".to_string(), opaque(harness).unwrap().into()),
            ("onChange".to_string(), method(function(vec![string()], void()), None).into()),
        ]
    };
    let a = local("t.idx", &LocalDef { version: 1, input: object(&[]), members: members("h@1") }).unwrap();
    let b = local("t.idx", &LocalDef { version: 1, input: object(&[]), members: members("h@2") }).unwrap();
    assert!(coverage(b.description(), a.description()).unwrap().contains("opaque"));
    assert!(fits(&function(vec![unknown()], void()), &function(vec![string()], void())));
    assert!(!fits(&function(vec![string()], void()), &function(vec![unknown()], void())));
}

#[test]
fn opaque_ids_hosts_and_headers_are_checked() {
    for bad in ["h", "@1", "h@", "h@1a", "a b@1", "h@x@1", "h\u{feff}@1"] {
        assert!(opaque(bad).is_err(), "{bad}");
    }
    assert_eq!(opaque("tools.harness@12").unwrap().id(), "tools.harness@12");
    assert!(host_named("").is_err() && host_family("", "k").is_err() && host_family("p:", "").is_err());
    let def = LocalDef { version: 0, input: object(&[]), members: Vec::new() };
    assert_eq!(local("t.x", &def).unwrap_err().to_string(), "contract version must be a positive integer");
    let both = RemoteDef {
        variants: vec![("v".into(), VariantDef::new().state("m", view()).method("m", method(None, None)))],
        ..chat_def()
    };
    assert_eq!(remote("t.chat", &both).unwrap_err().to_string(), "member m is both state and method in t.chat.v");
    let state_as_method =
        RemoteDef { variants: vec![("v".into(), VariantDef::new().method("m", view()))], ..chat_def() };
    assert_eq!(remote("t.chat", &state_as_method).unwrap_err().to_string(), "methods.m is not a method()");
}

#[test]
fn tokens_round_trip_and_bind_like_their_contract() {
    let chat = chat_v1();
    let token = Description::from_json(Json::parse(&chat.description().to_json().to_string()).unwrap()).unwrap();
    let received = contract_from_description(token).unwrap();
    assert_eq!(received.fingerprint(), chat.fingerprint());
    assert!(!takes_no_args(&chat, "send") && !takes_no_args(&chat, "view"));
    // A remote token without `host` is malformed.
    let mut raw = chat.description().to_json().as_object().unwrap().clone();
    raw.remove("host");
    let error = contract_from_description(Description::from_json(Json::object(raw)).unwrap()).unwrap_err();
    assert_eq!((error.code(), error.reason()), (Some(ChordErrorCode::Invalid), Some("token")));
    assert_eq!(error.to_string(), "invalid/token: malformed contract token");
}

/// A member from its description, built through the public API.
fn member(description: &Json) -> Member {
    let schema = |key: &str| description.get(key).cloned();
    match text(&description["kind"]) {
        "method" => method(schema("args"), schema("result")).into(),
        "state" => Member::State(schema("schema").unwrap()),
        _ => opaque(text(&description["id"])).unwrap().into(),
    }
}

fn members(list: &Json) -> Vec<(String, Member)> {
    let list = list.as_array().unwrap();
    list.iter().map(|entry| (text(&entry[0]).to_string(), member(&entry[1]))).collect()
}

fn build(def: &Json) -> Contract {
    let (name, version, input) = (text(&def["name"]), def["version"].as_i64().unwrap(), def["input"].clone());
    if text(&def["scope"]) == "local" {
        return local(name, &LocalDef { version, input, members: members(&def["members"]) }).unwrap();
    }
    let host = match def["host"].get("name") {
        Some(host) => host_named(text(host)).unwrap(),
        None => host_family(text(&def["host"]["prefix"]), text(&def["host"]["key"])).unwrap(),
    };
    let variants = def["variants"].as_array().unwrap().iter().map(|variant| {
        let state = members(&variant["state"]);
        (text(&variant["kind"]).to_string(), VariantDef { state, methods: members(&variant["methods"]) })
    });
    remote(name, &RemoteDef { version, input, host, variants: variants.collect() }).unwrap()
}

fn bound(provider: &Contract, accepts: Option<&HashMap<i64, ()>>, consumer: &Contract) -> Json {
    match bind(provider, accepts, consumer) {
        Ok(None) => json!("direct"),
        Ok(Some(_)) => json!("adapter"),
        Err(error) => json!({"code": error.code().unwrap().as_str(), "message": error.to_string()}),
    }
}

#[test]
fn descriptions_fingerprints_coverage_and_binding_match_upstream() {
    let fixture = fixture("contracts.json");
    let mut contracts = HashMap::new();
    for entry in fixture["contracts"].as_array().unwrap() {
        let label = text(&entry["label"]);
        let contract = build(&entry["def"]);
        assert_eq!(contract.description().to_json().to_string(), text(&entry["description"]), "{label}");
        assert_eq!(canonical_json(contract.description().to_json()), text(&entry["canonical"]), "{label}");
        assert_eq!(contract.fingerprint(), text(&entry["fingerprint"]), "{label}");
        let token = Description::from_json(Json::parse(text(&entry["description"])).unwrap()).unwrap();
        assert_eq!(contract_from_description(token).unwrap().fingerprint(), contract.fingerprint(), "{label}");
        contracts.insert(label.to_string(), contract);
    }
    let accepts = HashMap::from([(1, ())]);
    for pair in fixture["coverage"].as_array().unwrap() {
        let (provider, consumer) = (&contracts[text(&pair["provider"])], &contracts[text(&pair["consumer"])]);
        let label = format!("{} covers {}", text(&pair["provider"]), text(&pair["consumer"]));
        let actual = coverage(provider.description(), consumer.description()).map_or(Json::Null, Json::from);
        assert_eq!(actual, pair["coverage"], "{label}");
        assert_eq!(bound(provider, Some(&accepts), consumer), pair["bind"], "{label}");
        assert_eq!(bound(provider, None, consumer), pair["bindWithoutAdapters"], "{label}");
    }
    let index = &contracts["index"];
    assert!(takes_no_args(index, "10") && takes_no_args(index, "2"));
    assert!(!takes_no_args(index, "onChange") && !takes_no_args(index, "progress"));
    let members = index.description().variant("").unwrap();
    let names: Vec<_> = members.keys().map(String::as_str).collect();
    assert_eq!(names, ["2", "10", "harness", "onChange", "progress"]);
    assert!(matches!(members["harness"], MemberDescription::Opaque { ref id } if id == "h@1"));
}

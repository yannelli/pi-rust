//! Port of upstream `test/service-wire.test.ts`: control calls, parsers, state codecs, and endpoints.

mod support;

use std::sync::Arc;

use pi_chord::services::{
    RemoteServiceProvider, ServiceControlCall, ServiceDefinition, ServiceImpl, ServiceMode, ServiceProviderUpdate,
    ServiceStateDecoder, ServiceStateEncoder, ServiceSubscriptionSnapshot, create_remote_service_endpoint,
    create_service_catalogue_call, create_service_state_decoder, create_service_state_encoder,
    create_service_subscribe_call, create_service_unsubscribe_call, decode_service_control_call, define_service,
    parse_service_call, parse_service_catalogue, parse_service_provider_update, parse_service_subscription_snapshot,
    parse_wire_service_provider_update, parse_wire_service_subscription_snapshot,
};
use pi_chord::{BACKGROUND_CONTEXT, Json, json, replicated_state};
use support::{Recorder, matches_object};

type Parser = fn(&Json) -> pi_chord::ChordResult<()>;

fn snapshot(value: Json) -> ServiceSubscriptionSnapshot {
    parse_service_subscription_snapshot(&value).unwrap()
}

fn update(value: Json) -> ServiceProviderUpdate {
    parse_service_provider_update(&value).unwrap()
}

fn encode(encoder: &mut ServiceStateEncoder, value: &ServiceProviderUpdate) -> Json {
    encoder.encode_update(value).unwrap().to_json()
}

fn decode(decoder: &mut ServiceStateDecoder, wire: &Json) -> ServiceProviderUpdate {
    decoder.decode_update(&parse_wire_service_provider_update(wire).unwrap()).unwrap()
}

fn round_trip_snapshot(
    encoder: &mut ServiceStateEncoder,
    decoder: &mut ServiceStateDecoder,
    value: &ServiceSubscriptionSnapshot,
) -> ServiceSubscriptionSnapshot {
    decoder.decode_snapshot(&encoder.encode_snapshot(value).unwrap()).unwrap()
}

fn state_update(member: &str, sequence: u64, revision: u64) -> ServiceProviderUpdate {
    update(json!({"type": "state", "member": member, "sequence": sequence, "ops": [["s", ["revision"], revision]]}))
}

#[test]
fn encodes_control_calls_and_validates_service_values() {
    assert_eq!(decode_service_control_call(&create_service_catalogue_call()), Some(ServiceControlCall::Catalogue));
    let entries =
        json!([{"serviceId": "pi.models", "mode": "singleton"}, {"serviceId": "pi.dialogs", "mode": "keyed"}]);
    let catalogue = parse_service_catalogue(&entries).unwrap();
    assert_eq!(pi_chord::services::catalogue_to_json(&catalogue), entries);
    let subscribe = create_service_subscribe_call("subscription-1", "pi.models", ServiceMode::Singleton);
    assert_eq!(
        decode_service_control_call(&subscribe),
        Some(ServiceControlCall::Subscribe {
            subscription_id: "subscription-1".into(),
            service_id: "pi.models".into(),
            mode: ServiceMode::Singleton,
        })
    );
    assert_eq!(
        decode_service_control_call(&create_service_unsubscribe_call("subscription-1")),
        Some(ServiceControlCall::Unsubscribe { subscription_id: "subscription-1".into() })
    );
    let call = parse_service_call(&json!({
        "serviceId": "pi.question-dialog", "instance": {"key": "invocation-1", "generation": 2},
        "member": "submit", "args": [{"outcome": "selected", "index": 0}]
    }))
    .unwrap();
    assert_eq!(call.member, "submit");
}

#[test]
fn rejects_malformed_service_values() {
    let call = json!({"serviceId": "pi.models", "member": "list", "args": [], "extra": true});
    assert_eq!(parse_service_call(&call).unwrap_err().to_string(), "Invalid service call");
    let catalogue = json!([{"serviceId": "pi.models", "mode": "unknown"}]);
    assert_eq!(parse_service_catalogue(&catalogue).unwrap_err().to_string(), "Invalid service catalogue");
    let state = json!({"type": "state", "member": "state", "sequence": 0, "ops": []});
    assert_eq!(parse_service_provider_update(&state).unwrap_err().to_string(), "Invalid service state update");
    let wire = json!({"type": "state", "member": "state", "sequence": 1, "ops": [["?", 0]]});
    assert!(parse_wire_service_provider_update(&wire).is_err());
}

#[test]
fn validates_decoded_and_wire_snapshots_and_updates() {
    let value = json!({"serviceId": "pi.models", "mode": "singleton",
        "instances": [{"members": [{"name": "state", "kind": "state", "sequence": 0, "ops": [["r", {"revision": 1}]]}]}]});
    let parsed = snapshot(value.clone());
    assert_eq!(parsed.to_json(), value);
    let mut encoder = create_service_state_encoder();
    let wire = encoder.encode_snapshot(&parsed).unwrap();
    assert_eq!(parse_wire_service_subscription_snapshot(&wire.to_json()).unwrap(), wire);
    let state = json!({"type": "state", "member": "state", "sequence": 1, "ops": [["s", ["revision"], 2]]});
    let parsed = update(state.clone());
    assert_eq!(parsed.to_json(), state);
    let wire = encoder.encode_update(&parsed).unwrap();
    assert_eq!(parse_wire_service_provider_update(&wire.to_json()).unwrap(), wire);
}

#[test]
fn keeps_one_operation_codec_pair_for_one_subscription_state() {
    let (mut enc, mut dec) = (create_service_state_encoder(), create_service_state_decoder());
    let base = snapshot(json!({"serviceId": "pi.models", "mode": "singleton",
        "instances": [{"members": [{"name": "state", "kind": "state", "sequence": 0, "ops": [["r", {"revision": 0}]]}]}]}));
    assert_eq!(round_trip_snapshot(&mut enc, &mut dec, &base), base);
    let (first, second) = (state_update("state", 1, 1), state_update("state", 2, 2));
    let (first_wire, second_wire) = (encode(&mut enc, &first), encode(&mut enc, &second));
    assert!(matches_object(&first_wire, &json!({"ops": [["s", ["revision"], 1]]})));
    assert!(matches_object(&second_wire, &json!({"ops": [["#", 0, ["revision"]], ["s", 0, 2]]})));
    assert_eq!(decode(&mut dec, &first_wire), first);
    assert_eq!(decode(&mut dec, &second_wire), second);
}

#[test]
fn validates_explicit_resets_and_restarts_path_dictionaries_at_the_new_baseline() {
    let (mut enc, mut dec) = (create_service_state_encoder(), create_service_state_decoder());
    let base_json = json!({"serviceId": "pi.states", "mode": "singleton",
        "instances": [{"members": [{"name": "state", "kind": "state", "sequence": 0, "ops": [["r", {"before": 0}]]}]}]});
    round_trip_snapshot(&mut enc, &mut dec, &snapshot(base_json));
    for sequence in 1..=2 {
        let next = update(
            json!({"type": "state", "member": "state", "sequence": sequence, "ops": [["s", ["before"], sequence]]}),
        );
        decode(&mut dec, &encode(&mut enc, &next));
    }
    let reset_json = json!({"type": "reset", "snapshot": {"serviceId": "pi.states", "mode": "singleton",
        "instances": [{"members": [{"name": "state", "kind": "state", "sequence": 103, "ops": [["r", {"after": 103}]]}]}]}});
    let reset = update(reset_json.clone());
    assert_eq!(reset.to_json(), reset_json);
    let wire_reset = encode(&mut enc, &reset);
    assert_eq!(parse_wire_service_provider_update(&wire_reset).unwrap().to_json(), wire_reset);
    assert_eq!(decode(&mut dec, &wire_reset), reset);
    for sequence in 104..=106 {
        let next = update(
            json!({"type": "state", "member": "state", "sequence": sequence, "ops": [["s", ["after"], sequence]]}),
        );
        assert_eq!(decode(&mut dec, &encode(&mut enc, &next)), next);
    }
    let mut extra = reset_json.clone();
    extra.set("extra", true).unwrap();
    let partial = json!({"type": "reset", "snapshot": {"serviceId": "pi.states", "mode": "singleton",
        "instances": [{"members": [{"name": "state", "kind": "state", "sequence": 103, "ops": [["s", ["before"], 103]]}]}]}});
    let parsers: [Parser; 2] = [
        |value| parse_service_provider_update(value).map(drop),
        |value| parse_wire_service_provider_update(value).map(drop),
    ];
    for parse in parsers {
        assert!(parse(&extra).is_err());
        assert!(parse(&json!({"type": "reset", "snapshot": {}})).is_err());
        assert!(parse(&partial).unwrap_err().to_string().contains("full root replacements"));
    }
}

#[test]
fn isolates_operation_dictionaries_between_states_and_subscriptions() {
    let base = snapshot(json!({"serviceId": "pi.states", "mode": "singleton", "instances": [{"members": [
        {"name": "left", "kind": "state", "sequence": 0, "ops": [["r", {"revision": 0}]]},
        {"name": "right", "kind": "state", "sequence": 0, "ops": [["r", {"revision": 0}]]}
    ]}]}));
    let (mut first_enc, mut first_dec) = (create_service_state_encoder(), create_service_state_decoder());
    let (mut second_enc, mut second_dec) = (create_service_state_encoder(), create_service_state_decoder());
    round_trip_snapshot(&mut first_enc, &mut first_dec, &base);
    round_trip_snapshot(&mut second_enc, &mut second_dec, &base);
    let first_left = encode(&mut first_enc, &state_update("left", 1, 1));
    let first_right = encode(&mut first_enc, &state_update("right", 1, 1));
    let second_left = encode(&mut first_enc, &state_update("left", 2, 2));
    let second_right = encode(&mut first_enc, &state_update("right", 2, 2));
    let plain = json!({"ops": [["s", ["revision"], 1]]});
    let interned = json!({"ops": [["#", 0, ["revision"]], ["s", 0, 2]]});
    assert!(matches_object(&first_left, &plain));
    assert!(matches_object(&first_right, &plain));
    assert!(matches_object(&second_left, &interned));
    assert!(matches_object(&second_right, &interned));
    assert_eq!(decode(&mut first_dec, &first_left), state_update("left", 1, 1));
    assert_eq!(decode(&mut first_dec, &first_right), state_update("right", 1, 1));
    assert_eq!(decode(&mut first_dec, &second_left), state_update("left", 2, 2));
    assert_eq!(decode(&mut first_dec, &second_right), state_update("right", 2, 2));

    let independent = encode(&mut second_enc, &state_update("left", 1, 1));
    assert!(matches_object(&independent, &plain));
    assert_eq!(decode(&mut second_dec, &independent), state_update("left", 1, 1));

    let left_base = update(json!({"type": "state", "member": "left", "sequence": 3, "ops": [["r", {"revision": 3}]]}));
    assert_eq!(decode(&mut first_dec, &encode(&mut first_enc, &left_base)), left_base);
    let third_right = encode(&mut first_enc, &state_update("right", 3, 3));
    assert!(matches_object(&third_right, &json!({"ops": [["s", 0, 3]]})));
    assert_eq!(decode(&mut first_dec, &third_right), state_update("right", 3, 3));
}

#[test]
fn creates_and_removes_keyed_instance_codecs_with_their_lifecycle() {
    let (mut enc, mut dec) = (create_service_state_encoder(), create_service_state_decoder());
    let base = snapshot(json!({"serviceId": "pi.dialogs", "mode": "keyed", "instances": []}));
    assert_eq!(round_trip_snapshot(&mut enc, &mut dec, &base), base);
    let address = json!({"key": "dialog-1", "generation": 1});
    let spawned = update(json!({"type": "spawned", "instance": {"instance": address,
        "members": [{"name": "request", "kind": "state", "sequence": 0, "ops": [["r", {"value": 0}]]}]}}));
    assert_eq!(decode(&mut dec, &encode(&mut enc, &spawned)), spawned);
    let state = update(json!({"type": "state", "instance": address, "member": "request", "sequence": 1,
        "ops": [["s", ["value"], 1]]}));
    assert_eq!(decode(&mut dec, &encode(&mut enc, &state)), state);
    let closed = update(json!({"type": "closed", "instance": address}));
    assert_eq!(decode(&mut dec, &encode(&mut enc, &closed)), closed);
    let late = update(json!({"type": "state", "instance": address, "member": "request", "sequence": 2,
        "ops": [["s", ["value"], 1]]}));
    assert!(enc.encode_update(&late).unwrap_err().to_string().contains("Unknown service state"));
}

#[tokio::test]
async fn remote_service_endpoints_publish_and_clean_up_provider_subscriptions() {
    let counter = define_service::<()>("test.counter").unwrap();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&counter)]).unwrap();
    let state = replicated_state(json!({"value": 0}));
    provider.provide(&counter, ServiceImpl::new().state("state", &state)).unwrap();
    let endpoint = create_remote_service_endpoint(&provider);
    let updates = Recorder::new();
    let sink = updates.clone();
    let publish: pi_chord::services::ServiceUpdatePublisher = Arc::new(move |_, update, _| sink.push(update.to_json()));
    let catalogue = endpoint.invoke(create_service_catalogue_call(), publish.clone(), &BACKGROUND_CONTEXT).await;
    assert_eq!(catalogue.unwrap(), Some(json!([{"serviceId": "test.counter", "mode": "singleton"}])));
    let call = create_service_subscribe_call("subscription-1", "test.counter", ServiceMode::Singleton);
    let snapshot = endpoint.invoke(call, publish, &BACKGROUND_CONTEXT).await.unwrap().unwrap();
    assert!(matches_object(&snapshot, &json!({"serviceId": "test.counter", "mode": "singleton"})));
    state.change(&BACKGROUND_CONTEXT, |draft| draft.set("value", 1)).unwrap();
    assert_eq!(
        updates.get(),
        [json!({"type": "state", "member": "state", "sequence": 1, "ops": [["s", ["value"], 1]]})]
    );
    endpoint.dispose();
    state.change(&BACKGROUND_CONTEXT, |draft| draft.set("value", 2)).unwrap();
    assert_eq!(updates.len(), 1);
    provider.dispose().unwrap();
}

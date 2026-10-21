//! Helpers for the reload tests (upstream `test/reload.test.ts`): the greeter contract and its facet, roots that
//! run in the background, and short ways to reload, call and read state.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use pi_chord::host2::{
    Contract, Ctx, Declare, Facet, Host, Implementation, Initial, LoadOptions, Ref, ReloadOptions, ReloadReport,
    RootOptions, ServeOptions, StateEvent, StateOptions, define_facet, method, on_event,
};
use pi_chord::{ChordResult, Json, Unsubscribe, json};
use tokio::task::JoinHandle;

use super::{is, local_type, number, object, opt, req, string, text, tick, unknown, with};

pub type Bump = Arc<dyn Fn(&mut Json) -> ChordResult<()> + Send + Sync>;
pub type Extra = Arc<dyn Fn(&mut Declare, &Json) -> ChordResult<()> + Send + Sync>;

pub fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

pub fn greeter() -> Contract {
    let input = object(&[opt("id", with(string(), serde_json::json!({"default": "a"})))]);
    local_type("t.greeter", input, vec![("greet", method(string(), string()).into()), ("count", unknown().into())])
}

pub fn count_v1() -> Json {
    object(&[req("n", number())])
}

/// How a greeter facet serves (upstream `GreeterSpec`).
#[derive(Clone, Default)]
pub struct Spec {
    pub greeting: String,
    pub schema: Option<Json>,
    pub initial: Option<Json>,
    pub bump: Option<Bump>,
    pub options: ServeOptions,
    pub state: StateOptions,
    pub extra: Option<Extra>,
}

impl Spec {
    pub fn new(greeting: &str) -> Self {
        Self { greeting: greeting.to_string(), ..Self::default() }
    }

    pub fn schema(mut self, schema: Json, initial: Json) -> Self {
        (self.schema, self.initial) = (Some(schema), Some(initial));
        self
    }

    pub fn bump(mut self, bump: impl Fn(&mut Json) -> ChordResult<()> + Send + Sync + 'static) -> Self {
        self.bump = Some(Arc::new(bump));
        self
    }

    pub fn state(mut self, state: StateOptions) -> Self {
        self.state = state;
        self
    }

    pub fn options(mut self, options: ServeOptions) -> Self {
        self.options = options;
        self
    }

    pub fn extra(mut self, extra: impl Fn(&mut Declare, &Json) -> ChordResult<()> + Send + Sync + 'static) -> Self {
        self.extra = Some(Arc::new(extra));
        self
    }
}

fn count_up(draft: &mut Json) -> ChordResult<()> {
    let n = draft["n"].as_f64().unwrap_or_default();
    draft.set("n", n + 1.0)
}

pub fn greeter_facet(spec: Spec) -> Facet {
    define_facet("greeter", move |api| {
        let spec = spec.clone();
        let factory = move |input: &Json, _: &Json, decl: &mut Declare| {
            if let Some(extra) = &spec.extra {
                extra(decl, input)?;
            }
            let schema = spec.schema.clone().unwrap_or_else(count_v1);
            let first = Initial::Value(spec.initial.clone().unwrap_or_else(|| json!({"n": 0})));
            let count = decl.state_with("count", schema, first, spec.state.clone())?;
            let (greeting, bump, counted) = (spec.greeting.clone(), spec.bump.clone(), count.clone());
            Ok(Implementation::new().state("count", count).method("greet", move |name, ctx| {
                let (count, greeting, bump) = (counted.clone(), greeting.clone(), bump.clone());
                async move {
                    count.change(&ctx, |draft| match &bump {
                        Some(bump) => bump(draft),
                        None => count_up(draft),
                    })?;
                    Ok(json!(format!("{greeting} {}", text(&name))))
                }
            }))
        };
        api.serve(&greeter(), factory, spec.options.clone())
    })
}

pub async fn install(host: &Host, facet: Facet) {
    host.install(&facet, LoadOptions::default()).await.unwrap();
}

pub fn hold(host: &Host, contract: &Contract, input: Json) -> Ref {
    host.hold(contract, input, Default::default()).unwrap()
}

pub fn hold_greeter(host: &Host, input: Json) -> Ref {
    hold(host, &greeter(), input)
}

pub async fn greet(reference: &Ref, name: &str) -> ChordResult<String> {
    Ok(text(&reference.call_json("greet", json!(name), None).await?).to_string())
}

pub async fn greet_in(reference: &Ref, name: &str, ctx: &Ctx) -> ChordResult<String> {
    Ok(text(&reference.call_json("greet", json!(name), Some(ctx)).await?).to_string())
}

/// Reloads one facet with default options.
pub fn reload(host: &Host, facet: Facet) -> impl Future<Output = ChordResult<ReloadReport>> + Send + use<> {
    host.reload(vec![facet.into()], ReloadOptions::default())
}

/// Starts a root now; it runs in the background (upstream's `host.root` starts at once).
pub fn spawn_root<F, Fut>(host: &Host, run: F) -> JoinHandle<ChordResult<Json>>
where
    F: FnOnce(Ctx) -> Fut + Send + 'static,
    Fut: Future<Output = ChordResult<Json>> + Send + 'static,
{
    tokio::spawn(host.root(run, RootOptions::default()))
}

pub fn state_of(reference: &Ref, name: &str) -> Json {
    reference.state(name).unwrap().value().unwrap()
}

pub fn state_events(
    reference: &Ref,
    name: &str,
    listener: impl Fn(&StateEvent) + Send + Sync + 'static,
) -> Unsubscribe {
    on_event(&*reference.state(name).unwrap(), listener)
}

/// Ticks until a call through `reference` is refused `upgrade_conflict` (the gate is up).
pub async fn until_conflict(reference: &Ref) {
    for _ in 0..200 {
        match greet(reference, "probe").await {
            Err(error) if is(&error, "upgrade_conflict") => return,
            _ => tick().await,
        }
    }
    panic!("the gate never came up");
}

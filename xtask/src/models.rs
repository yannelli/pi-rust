//! `cargo xtask models <data-dir> [--out <path>]`: flatten the upstream per-provider catalog
//! (`packages/ai/src/providers/data/*.json`) into the bundled `crates/pi-ai/models.json`, and copy
//! the generation time of `.manifest.json` to `models.manifest.json` beside it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Model kinds the upstream catalog knows, in report order.
const MODEL_TYPES: [&str; 3] = ["chat", "image", "classifier"];

pub fn run(args: &[String], root: &Path) -> Result<(), String> {
    let mut dir = None;
    let mut out = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--out" => out = Some(PathBuf::from(rest.next().ok_or("--out needs a path")?)),
            flag if flag.starts_with("--") => return Err(format!("models: unknown flag {flag}")),
            _ if dir.is_none() => dir = Some(PathBuf::from(arg)),
            _ => return Err(format!("models: unexpected argument {arg}")),
        }
    }
    let dir = dir.ok_or("models needs the upstream data directory")?;
    let out = out.unwrap_or_else(|| root.join("crates/pi-ai/models.json"));
    let catalog = flatten_dir(&dir)?;
    let manifest = manifest_json(&dir)?;
    std::fs::write(&out, render(&catalog.models)).map_err(|error| format!("{}: {error}", out.display()))?;
    let manifest_out = out.with_file_name("models.manifest.json");
    std::fs::write(&manifest_out, manifest).map_err(|error| format!("{}: {error}", manifest_out.display()))?;
    let types: Vec<String> = MODEL_TYPES.iter().map(|kind| format!("{kind} {}", catalog.count(kind))).collect();
    println!(
        "Wrote {} models ({}) from {} providers to {}",
        catalog.models.len(),
        types.join(", "),
        catalog.providers,
        out.display()
    );
    Ok(())
}

/// `{"generatedAt": ...}` from the `.manifest.json` upstream writes with its provider files.
pub fn manifest_json(dir: &Path) -> Result<String, String> {
    let path = dir.join(".manifest.json");
    let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let manifest: Value = serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let generated_at = manifest.get("generatedAt").and_then(Value::as_str).filter(|stamp| !stamp.is_empty());
    let generated_at = generated_at.ok_or_else(|| format!("{}: no generatedAt", path.display()))?;
    Ok(format!("{}\n", serde_json::json!({ "generatedAt": generated_at })))
}

/// The flattened catalog: models in output order plus the number of provider files read.
#[derive(Debug)]
pub struct Catalog {
    pub models: Vec<Map<String, Value>>,
    pub providers: usize,
}

impl Catalog {
    fn count(&self, kind: &str) -> usize {
        self.models.iter().filter(|model| model.get("type").and_then(Value::as_str) == Some(kind)).count()
    }

    /// Model counts per provider and type, keyed by provider.
    #[cfg(test)]
    pub fn counts(&self) -> BTreeMap<String, BTreeMap<String, usize>> {
        let mut counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        for model in &self.models {
            let field = |name: &str| model.get(name).and_then(Value::as_str).unwrap_or_default().to_string();
            *counts.entry(field("provider")).or_default().entry(field("type")).or_default() += 1;
        }
        counts
    }
}

/// Provider files in `dir`: every `*.json` except dot-files, sorted by file name.
pub fn provider_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let mut files = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".json") && !name.starts_with('.') && entry.path().is_file() {
            files.insert(name, entry.path());
        }
    }
    Ok(files.into_values().collect())
}

/// Read and flatten every provider file in `dir`, in file name order.
pub fn flatten_dir(dir: &Path) -> Result<Catalog, String> {
    let files = provider_files(dir)?;
    if files.is_empty() {
        return Err(format!("{}: no provider *.json files", dir.display()));
    }
    let mut models = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let provider = name.trim_end_matches(".json");
        models.extend(flatten_provider(&name, provider, &text)?);
    }
    Ok(Catalog { models, providers: files.len() })
}

/// Flatten one `{ "<api>": { "<type>:<id>": model } }` document, keeping group and model order.
pub fn flatten_provider(file: &str, provider: &str, text: &str) -> Result<Vec<Map<String, Value>>, String> {
    let document: Value = serde_json::from_str(text).map_err(|error| format!("{file}: invalid JSON: {error}"))?;
    let Value::Object(groups) = document else {
        return Err(format!("{file}: expected an object of API groups"));
    };
    let mut models = Vec::new();
    for (api, group) in groups {
        let Value::Object(group) = group else {
            return Err(format!("{file}: group {api:?} is not an object"));
        };
        for (key, model) in group {
            let Value::Object(model) = model else {
                return Err(format!("{file}: {api} / {key}: model is not an object"));
            };
            check_model(file, provider, &api, &key, &model)?;
            models.push(model);
        }
    }
    Ok(models)
}

fn check_model(file: &str, provider: &str, api: &str, key: &str, model: &Map<String, Value>) -> Result<(), String> {
    let field = |name: &str| {
        model
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{file}: {api} / {key}: missing string field {name:?}"))
    };
    let (model_api, kind, id, model_provider) = (field("api")?, field("type")?, field("id")?, field("provider")?);
    if model_api != api {
        return Err(format!("{file}: {api} / {key}: api is {model_api:?} but the group is {api:?}"));
    }
    if !MODEL_TYPES.contains(&kind) {
        return Err(format!("{file}: {api} / {key}: unknown model type {kind:?}"));
    }
    if key != format!("{kind}:{id}") {
        return Err(format!("{file}: {api} / {key}: key does not match \"{kind}:{id}\""));
    }
    if model_provider != provider {
        return Err(format!("{file}: {api} / {key}: provider is {model_provider:?} but the file is for {provider:?}"));
    }
    Ok(())
}

/// The bundled layout: `[`, one compact model per line joined by `,\n`, then `]` and a newline.
pub fn render(models: &[Map<String, Value>]) -> String {
    if models.is_empty() {
        return "[]\n".into();
    }
    let lines: Vec<String> = models
        .iter()
        .map(|model| {
            let mut line = String::new();
            write_object(&mut line, model);
            line
        })
        .collect();
    format!("[\n{}\n]\n", lines.join(",\n"))
}

/// Compact JSON as `JSON.stringify` writes it, so upstream numbers keep their text.
fn write_json(out: &mut String, value: &Value) {
    match value {
        Value::Object(map) => write_object(out, map),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(out, item);
            }
            out.push(']');
        }
        Value::Number(number) => match number.as_f64().filter(|_| number.is_f64()) {
            Some(float) => out.push_str(&js_number(float)),
            None => out.push_str(&number.to_string()),
        },
        other => out.push_str(&other.to_string()),
    }
}

fn write_object(out: &mut String, map: &Map<String, Value>) {
    out.push('{');
    for (index, (key, item)) in map.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&Value::String(key.clone()).to_string());
        out.push(':');
        write_json(out, item);
    }
    out.push('}');
}

/// ECMAScript `Number.prototype.toString` for a finite value: the shortest round-trip digits,
/// in plain notation from 1e-6 up to 1e21 and exponent notation (`1e-7`, `1.5e+21`) outside.
fn js_number(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits = mantissa.replace('.', "");
    let k = digits.len() as i32;
    let n = exponent.parse::<i32>().unwrap_or_default() + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat(-n as usize))
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() { String::new() } else { format!(".{rest}") };
        let sign = if n > 0 { '+' } else { '-' };
        format!("{first}{fraction}e{sign}{}", (n - 1).abs())
    };
    if value < 0.0 { format!("-{body}") } else { body }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, api: &str, kind: &str, id: &str) -> String {
        format!(
            r#""{kind}:{id}":{{"id":"{id}","name":"{id} name","api":"{api}","provider":"{provider}","baseUrl":"https://example.test","type":"{kind}"}}"#
        )
    }

    fn fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    fn ids(catalog: &Catalog) -> Vec<String> {
        catalog.models.iter().map(|model| format!("{}/{}", model["provider"], model["id"]).replace('"', "")).collect()
    }

    #[test]
    fn flattens_in_file_group_and_model_order() {
        let beta = format!(
            r#"{{"zeta-api":{{{},{}}},"alpha-api":{{{}}}}}"#,
            model("beta", "zeta-api", "chat", "z2"),
            model("beta", "zeta-api", "image", "z1"),
            model("beta", "alpha-api", "classifier", "a1"),
        );
        let alpha = format!(r#"{{"x":{{{}}}}}"#, model("alpha", "x", "chat", "m"));
        let dir = fixture(&[
            ("beta.json", &beta),
            ("alpha.json", &alpha),
            (".manifest.json", r#"{"files":{}}"#),
            ("notes.txt", "ignored"),
        ]);
        let catalog = flatten_dir(dir.path()).unwrap();
        assert_eq!(catalog.providers, 2);
        assert_eq!(ids(&catalog), ["alpha/m", "beta/z2", "beta/z1", "beta/a1"]);
        assert_eq!(catalog.count("chat"), 2);
        assert_eq!(catalog.counts()["beta"]["image"], 1);
    }

    #[test]
    fn copies_the_manifest_generation_time() {
        let stamp = r#"{"schemaVersion":6,"generatedAt":"2026-10-31T05:41:14.278Z","files":{}}"#;
        let dir = fixture(&[(".manifest.json", stamp)]);
        assert_eq!(manifest_json(dir.path()).unwrap(), "{\"generatedAt\":\"2026-10-31T05:41:14.278Z\"}\n");
        let error = manifest_json(fixture(&[(".manifest.json", r#"{"files":{}}"#)]).path()).unwrap_err();
        assert!(error.contains("no generatedAt"), "{error}");
        assert!(manifest_json(fixture(&[]).path()).unwrap_err().contains(".manifest.json"));
    }

    #[test]
    fn keeps_every_field_verbatim_in_upstream_order() {
        let text = r#"{"api":{"chat:m":{"type":"chat","id":"m","lab":"x","api":"api","provider":"p","cost":{"output":0.000001,"input":12.5},"enabled":true,"thinkingLevelMap":{"off":null},"contextWindow":1000000}}}"#;
        let models = flatten_provider("p.json", "p", text).unwrap();
        let rendered = render(&models);
        let expected = r#"{"type":"chat","id":"m","lab":"x","api":"api","provider":"p","cost":{"output":0.000001,"input":12.5},"enabled":true,"thinkingLevelMap":{"off":null},"contextWindow":1000000}"#;
        assert_eq!(rendered, format!("[\n{expected}\n]\n"));
    }

    #[test]
    fn numbers_match_javascript_text() {
        let cases = [
            (0.0, "0"),
            (12.5, "12.5"),
            (-0.25, "-0.25"),
            (0.000001, "0.000001"),
            (0.0000015, "0.0000015"),
            (1e-7, "1e-7"),
            (2.5e-8, "2.5e-8"),
            (1234.5678, "1234.5678"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e300, "1.5e+300"),
        ];
        for (value, text) in cases {
            assert_eq!(js_number(value), text, "{value:e}");
        }
    }

    #[test]
    fn renders_one_model_per_line() {
        let text = format!(r#"{{"a":{{{},{}}}}}"#, model("p", "a", "chat", "one"), model("p", "a", "chat", "two"));
        let rendered = render(&flatten_provider("p.json", "p", &text).unwrap());
        let lines: Vec<&str> = rendered.split('\n').collect();
        assert_eq!(lines.len(), 5);
        assert_eq!((lines[0], lines[3], lines[4]), ("[", "]", ""));
        assert!(lines[1].starts_with(r#"{"id":"one","#) && lines[1].ends_with("},"));
        assert!(lines[2].starts_with(r#"{"id":"two","#) && lines[2].ends_with('}'));
        let parsed: Vec<Value> = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(render(&[]), "[]\n");
    }

    #[test]
    fn rejects_mismatched_api_key_and_provider() {
        let wrong_api = format!(r#"{{"other":{{{}}}}}"#, model("p", "a", "chat", "m"));
        let error = flatten_provider("p.json", "p", &wrong_api).unwrap_err();
        assert!(error.contains("p.json") && error.contains("chat:m") && error.contains("api is \"a\""), "{error}");

        let wrong_key = format!(r#"{{"a":{{{}}}}}"#, model("p", "a", "chat", "m").replacen("chat:m", "image:m", 1));
        let error = flatten_provider("p.json", "p", &wrong_key).unwrap_err();
        assert!(error.contains("image:m") && error.contains("does not match \"chat:m\""), "{error}");

        let wrong_provider = format!(r#"{{"a":{{{}}}}}"#, model("q", "a", "chat", "m"));
        let dir = fixture(&[("p.json", &wrong_provider)]);
        let error = flatten_dir(dir.path()).unwrap_err();
        assert!(error.contains("p.json") && error.contains("provider is \"q\""), "{error}");

        let unknown_type = format!(r#"{{"a":{{{}}}}}"#, model("p", "a", "audio", "m"));
        let error = flatten_provider("p.json", "p", &unknown_type).unwrap_err();
        assert!(error.contains("unknown model type \"audio\""), "{error}");

        let missing_id = r#"{"a":{"chat:m":{"api":"a","provider":"p","type":"chat"}}}"#;
        let error = flatten_provider("p.json", "p", missing_id).unwrap_err();
        assert!(error.contains("missing string field \"id\""), "{error}");
        assert!(flatten_provider("p.json", "p", "[]").unwrap_err().contains("p.json"));
        assert!(flatten_dir(fixture(&[]).path()).unwrap_err().contains("no provider"));
    }
}

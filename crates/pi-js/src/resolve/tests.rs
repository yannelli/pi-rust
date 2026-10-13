use std::fs;
use std::path::Path;

use super::*;

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn no_virtual(_: &str) -> Option<String> {
    None
}

fn file(resolved: Result<Resolved, JsError>) -> PathBuf {
    match resolved.unwrap() {
        Resolved::File(path) => path,
        other => panic!("expected a file, got {other:?}"),
    }
}

#[test]
fn resolves_relative_files_with_typescript_conventions() {
    let dir = tempfile::tempdir().unwrap();
    let root = canonical(dir.path());
    write(&root, "ext/index.ts", "");
    write(&root, "ext/util.ts", "");
    write(&root, "ext/lib/index.ts", "");
    write(&root, "ext/data.json", "{}");
    write(&root, "ext/both.js", "");
    write(&root, "ext/both.ts", "");
    let resolver = ModuleResolver::new();
    let from = root.join("ext/index.ts");
    let resolve = |spec: &str| file(resolver.resolve(spec, Some(&from), &root, ResolveKind::Import, &no_virtual));
    assert_eq!(resolve("./util.js"), root.join("ext/util.ts"));
    assert_eq!(resolve("./util"), root.join("ext/util.ts"));
    assert_eq!(resolve("./lib"), root.join("ext/lib/index.ts"));
    assert_eq!(resolve("./data.json"), root.join("ext/data.json"));
    assert_eq!(resolve("./both"), root.join("ext/both.ts"), "TypeScript importers prefer .ts");
    let js_from = root.join("ext/main.js");
    let js = file(resolver.resolve("./both", Some(&js_from), &root, ResolveKind::Import, &no_virtual));
    assert_eq!(js, root.join("ext/both.js"));
    let url = url::Url::from_file_path(root.join("ext/util.ts")).unwrap();
    assert_eq!(resolve(url.as_str()), root.join("ext/util.ts"));
}

#[test]
fn resolves_packages_through_node_modules_and_exports() {
    let dir = tempfile::tempdir().unwrap();
    let root = canonical(dir.path());
    write(&root, "proj/ext/index.ts", "");
    write(
        &root,
        "proj/node_modules/dual/package.json",
        r#"{"name":"dual","exports":{".":{"import":"./esm/index.js","require":"./cjs/index.cjs"},"./feature/*":"./lib/*.js","./package.json":"./package.json"}}"#,
    );
    write(&root, "proj/node_modules/dual/esm/index.js", "");
    write(&root, "proj/node_modules/dual/cjs/index.cjs", "");
    write(&root, "proj/node_modules/dual/lib/a.js", "");
    write(&root, "node_modules/@scope/plain/package.json", r#"{"name":"@scope/plain","main":"lib/main"}"#);
    write(&root, "node_modules/@scope/plain/lib/main.js", "");
    write(&root, "node_modules/@scope/plain/extra/thing.js", "");
    write(&root, "proj/node_modules/modfield/package.json", r#"{"main":"./main.cjs","module":"./module.mjs"}"#);
    write(&root, "proj/node_modules/modfield/main.cjs", "");
    write(&root, "proj/node_modules/modfield/module.mjs", "");
    write(&root, "proj/node_modules/noindex/index.js", "");
    let resolver = ModuleResolver::new();
    let from = root.join("proj/ext/index.ts");
    let import = |spec: &str| file(resolver.resolve(spec, Some(&from), &root, ResolveKind::Import, &no_virtual));
    let require = |spec: &str| file(resolver.resolve(spec, Some(&from), &root, ResolveKind::Require, &no_virtual));
    assert_eq!(import("dual"), root.join("proj/node_modules/dual/esm/index.js"));
    assert_eq!(require("dual"), root.join("proj/node_modules/dual/cjs/index.cjs"));
    assert_eq!(import("dual/feature/a"), root.join("proj/node_modules/dual/lib/a.js"));
    let hidden = resolver.resolve("dual/lib/a.js", Some(&from), &root, ResolveKind::Import, &no_virtual);
    assert_eq!(hidden.unwrap_err().code.as_deref(), Some("ERR_PACKAGE_PATH_NOT_EXPORTED"));
    assert_eq!(import("@scope/plain"), root.join("node_modules/@scope/plain/lib/main.js"));
    assert_eq!(import("@scope/plain/extra/thing"), root.join("node_modules/@scope/plain/extra/thing.js"));
    assert_eq!(import("modfield"), root.join("proj/node_modules/modfield/module.mjs"));
    assert_eq!(require("modfield"), root.join("proj/node_modules/modfield/main.cjs"));
    assert_eq!(import("noindex"), root.join("proj/node_modules/noindex/index.js"));
    let missing = resolver.resolve("absent", Some(&from), &root, ResolveKind::Require, &no_virtual).unwrap_err();
    assert_eq!(missing.code.as_deref(), Some("MODULE_NOT_FOUND"));
    assert!(missing.message.contains("Cannot find module 'absent'"));
}

#[test]
fn resolves_builtins_virtual_modules_and_imports() {
    let dir = tempfile::tempdir().unwrap();
    let root = canonical(dir.path());
    write(
        &root,
        "pkg/package.json",
        r##"{"name":"me","imports":{"#util":"./src/util.ts"},"exports":"./src/index.ts"}"##,
    );
    write(&root, "pkg/src/index.ts", "");
    write(&root, "pkg/src/util.ts", "");
    let resolver = ModuleResolver::new();
    let from = root.join("pkg/src/index.ts");
    let virtual_name = |spec: &str| (spec == "@mariozechner/pi-tui").then(|| "@earendil-works/pi-tui".to_string());
    let resolve = |spec: &str| resolver.resolve(spec, Some(&from), &root, ResolveKind::Import, &virtual_name);
    assert_eq!(resolve("fs").unwrap(), Resolved::Builtin("node:fs".into()));
    assert_eq!(resolve("node:fs/promises").unwrap(), Resolved::Builtin("node:fs/promises".into()));
    assert_eq!(resolve("pi:host").unwrap(), Resolved::Builtin("pi:host".into()));
    assert_eq!(resolve("node:nope").unwrap_err().code.as_deref(), Some("ERR_UNKNOWN_BUILTIN_MODULE"));
    assert_eq!(resolve("@mariozechner/pi-tui").unwrap(), Resolved::Virtual("@earendil-works/pi-tui".into()));
    assert_eq!(resolve("#util").unwrap(), Resolved::File(root.join("pkg/src/util.ts")));
    assert_eq!(resolve("me").unwrap(), Resolved::File(root.join("pkg/src/index.ts")));
}

#[test]
fn splits_package_names() {
    assert_eq!(split_package("a"), Some(("a", ".".into())));
    assert_eq!(split_package("a/b/c"), Some(("a", "./b/c".into())));
    assert_eq!(split_package("@s/a"), Some(("@s/a", ".".into())));
    assert_eq!(split_package("@s/a/b"), Some(("@s/a", "./b".into())));
    assert_eq!(split_package("@s"), None);
    assert_eq!(normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
}

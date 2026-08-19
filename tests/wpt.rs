//! Minimal, pinned Web Platform Test harness for the Phase 1 public slice.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use deno_audio::{AudioExtensionOptions, AudioHost};
use deno_core::serde_json::{self, Value};
use deno_core::{op2, JsRuntime, OpState, RuntimeOptions, ToJsBuffer};
use deno_error::JsErrorBox;

const WPT_COMMIT: &str = "fbc1b90e28b38e919acabeeb14d1b074f2c125ec";
const MAX_WPT_RESOURCE_BYTES: u64 = 16 * 1024 * 1024;
const EXPECTATIONS: &str = include_str!("wpt/expectations.json");
const HARNESS_ADAPTER: &str = include_str!("wpt/harness.js");

#[derive(Clone)]
struct WptRoot(PathBuf);

#[op2]
#[serde]
fn op_wpt_read_file(
    state: &mut OpState,
    #[string] request: &str,
) -> Result<ToJsBuffer, JsErrorBox> {
    let root = state.borrow::<WptRoot>();
    read_wpt_resource(&root.0, request).map(Into::into)
}

deno_core::extension!(deno_audio_wpt, ops = [op_wpt_read_file]);

#[derive(Debug)]
enum HtmlScript {
    External(String),
    Inline(String),
}

#[derive(Debug)]
struct Expectation {
    path: String,
    category: String,
    status: String,
    reason: Option<String>,
}

fn type_error(message: impl Into<String>) -> JsErrorBox {
    JsErrorBox::type_error(message.into())
}

fn percent_decode(input: &str) -> Result<String, JsErrorBox> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return Err(type_error("malformed percent escape in WPT resource URL"));
        }
        let high = (bytes[index + 1] as char)
            .to_digit(16)
            .ok_or_else(|| type_error("malformed percent escape in WPT resource URL"))?;
        let low = (bytes[index + 2] as char)
            .to_digit(16)
            .ok_or_else(|| type_error("malformed percent escape in WPT resource URL"))?;
        decoded.push(
            u8::try_from((high << 4) | low).expect("two hexadecimal digits always fit in one byte"),
        );
        index += 3;
    }
    String::from_utf8(decoded).map_err(|_| type_error("WPT resource URL is not UTF-8"))
}

fn request_path(request: &str) -> Result<PathBuf, JsErrorBox> {
    let path_with_suffix = if let Some(scheme) = request.find("://") {
        let protocol = &request[..scheme];
        let after_authority = &request[scheme + 3..];
        let (authority, path) = after_authority
            .find('/')
            .map_or((after_authority, "/"), |slash| {
                (&after_authority[..slash], &after_authority[slash..])
            });
        let host = authority.split(':').next().unwrap_or("");
        if !matches!(protocol, "http" | "https")
            || !(host == "web-platform.test" || host.ends_with(".web-platform.test"))
        {
            return Err(type_error("WPT fetch only serves the pinned test origin"));
        }
        path
    } else {
        request
    };
    let suffix = path_with_suffix
        .find(['?', '#'])
        .unwrap_or(path_with_suffix.len());
    let decoded = percent_decode(&path_with_suffix[..suffix])?;
    if decoded.contains(['\\', '\0']) {
        return Err(type_error("invalid WPT resource path"));
    }

    let mut relative = PathBuf::new();
    for component in Path::new(decoded.trim_start_matches('/')).components() {
        match component {
            Component::Normal(value) => relative.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(type_error("WPT resource path escapes the pinned checkout"));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(type_error("WPT resource URL does not name a file"));
    }
    Ok(relative)
}

fn read_wpt_resource(root: &Path, request: &str) -> Result<Vec<u8>, JsErrorBox> {
    let relative = request_path(request)?;
    let candidate = root.join(relative);
    let canonical = candidate
        .canonicalize()
        .map_err(|error| type_error(format!("unable to read WPT resource: {error}")))?;
    if !canonical.starts_with(root) {
        return Err(type_error("WPT resource path escapes the pinned checkout"));
    }
    let metadata = canonical
        .metadata()
        .map_err(|error| type_error(format!("unable to inspect WPT resource: {error}")))?;
    if !metadata.is_file() || metadata.len() > MAX_WPT_RESOURCE_BYTES {
        return Err(type_error("WPT resource is not a bounded regular file"));
    }
    std::fs::read(canonical)
        .map_err(|error| type_error(format!("unable to read WPT resource: {error}")))
}

fn string_field(object: &Value, field: &str) -> String {
    object[field]
        .as_str()
        .unwrap_or_else(|| panic!("expectation field {field:?} must be a string"))
        .to_owned()
}

fn parse_expectations() -> (String, Vec<Expectation>) {
    let manifest: Value =
        serde_json::from_str(EXPECTATIONS).expect("expectations.json must be valid JSON");
    let commit = string_field(&manifest, "wpt_commit");
    let tests = manifest["tests"]
        .as_array()
        .expect("expectations.json tests must be an array")
        .iter()
        .map(|entry| Expectation {
            path: string_field(entry, "path"),
            category: string_field(entry, "category"),
            status: string_field(entry, "status"),
            reason: entry["reason"].as_str().map(str::to_owned),
        })
        .collect();
    (commit, tests)
}

fn configured_wpt_root() -> Option<PathBuf> {
    let candidate = std::env::var_os("DENO_AUDIO_WPT_ROOT").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/wpt"),
        PathBuf::from,
    );
    candidate.exists().then(|| {
        candidate
            .canonicalize()
            .expect("the configured WPT checkout must be readable")
    })
}

fn verify_wpt_commit(root: &Path, expected: &str) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git must be available to verify the WPT pin");
    assert!(
        output.status.success(),
        "configured WPT root is not a Git checkout"
    );
    let actual = String::from_utf8(output.stdout).expect("git commit output must be UTF-8");
    assert_eq!(
        actual.trim(),
        expected,
        "configured WPT checkout is not pinned"
    );
}

fn script_src(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut rest = 0;
    while let Some(offset) = lower[rest..].find("src") {
        let start = rest + offset;
        let before_ok = start == 0 || lower.as_bytes()[start - 1].is_ascii_whitespace();
        let after = &tag[start + 3..];
        let trimmed = after.trim_start();
        if before_ok && trimmed.starts_with('=') {
            let value = trimmed[1..].trim_start();
            let first = value.chars().next()?;
            if first == '\'' || first == '"' {
                let tail = &value[first.len_utf8()..];
                return tail.find(first).map(|end| tail[..end].to_owned());
            }
            let end = value
                .find(|character: char| character.is_ascii_whitespace() || character == '>')
                .unwrap_or(value.len());
            return Some(value[..end].to_owned());
        }
        rest = start + 3;
    }
    None
}

fn html_scripts(html: &str) -> Result<Vec<HtmlScript>, String> {
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0;
    let mut scripts = Vec::new();
    while let Some(relative_start) = lower[cursor..].find("<script") {
        let start = cursor + relative_start;
        let tag_end = lower[start..]
            .find('>')
            .map(|offset| start + offset)
            .ok_or_else(|| "unterminated <script> tag".to_owned())?;
        let close = lower[tag_end + 1..]
            .find("</script>")
            .map(|offset| tag_end + 1 + offset)
            .ok_or_else(|| "unterminated <script> body".to_owned())?;
        let tag = &html[start..=tag_end];
        if let Some(src) = script_src(tag) {
            scripts.push(HtmlScript::External(src));
        } else {
            scripts.push(HtmlScript::Inline(html[tag_end + 1..close].to_owned()));
        }
        cursor = close + "</script>".len();
    }
    Ok(scripts)
}

fn resolve_script(page: &Path, source: &str) -> Result<PathBuf, String> {
    if source.contains("://") {
        return Err(format!("remote WPT script is not allowed: {source}"));
    }
    let suffix = source.find(['?', '#']).unwrap_or(source.len());
    let decoded = percent_decode(&source[..suffix]).map_err(|error| error.to_string())?;
    if decoded.contains(['\\', '\0']) {
        return Err("invalid WPT script path".to_owned());
    }
    let mut resolved = if source.starts_with('/') {
        PathBuf::new()
    } else {
        page.parent().unwrap_or_else(|| Path::new("")).to_owned()
    };
    for component in Path::new(decoded.trim_start_matches('/')).components() {
        match component {
            Component::Normal(value) => resolved.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    return Err("WPT script path escapes the pinned checkout".to_owned());
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("invalid WPT script path".to_owned());
            }
        }
    }
    Ok(resolved)
}

fn wpt_runtime(root: &Path) -> JsRuntime {
    let host = Arc::new(AudioHost::default());
    let runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio_wpt::init(),
            deno_audio::deno_audio::init(AudioExtensionOptions::new(host)),
        ],
        unsafe_expose_natives_and_gc: true,
        ..RuntimeOptions::default()
    });
    runtime
        .op_state()
        .borrow_mut()
        .put(WptRoot(root.to_owned()));
    runtime
}

async fn run_wpt_file(root: &Path, relative: &str) {
    let page = PathBuf::from(relative);
    let html = String::from_utf8(
        read_wpt_resource(root, relative).expect("expected WPT file must be readable"),
    )
    .expect("selected WPT HTML must be UTF-8");
    let scripts = html_scripts(&html).expect("selected WPT HTML must have parseable scripts");
    let mut runtime = wpt_runtime(root);

    runtime
        .execute_script("deno_audio_wpt_harness.js", HARNESS_ADAPTER)
        .expect("WPT adapter must install");

    let mut installed_callbacks = false;
    for (index, script) in scripts.into_iter().enumerate() {
        match script {
            HtmlScript::External(source) if source.ends_with("testharnessreport.js") => {}
            HtmlScript::External(source) => {
                let script_path = resolve_script(&page, &source).expect("script URL must be local");
                let script_path_string = script_path.to_string_lossy().replace('\\', "/");
                let code = String::from_utf8(
                    read_wpt_resource(root, &script_path_string)
                        .expect("WPT support script must be readable"),
                )
                .expect("WPT support script must be UTF-8");
                runtime
                    .execute_script(format!("wpt:///{script_path_string}"), code)
                    .unwrap_or_else(|error| panic!("failed to load {script_path_string}: {error}"));
                if source.ends_with("testharness.js") {
                    runtime
                        .execute_script(
                            "deno_audio_wpt_callbacks.js",
                            format!(
                                "__denoAudioInstallWptCallbacks({});",
                                serde_json::to_string(relative).expect("WPT path must serialize")
                            ),
                        )
                        .expect("WPT result callbacks must install");
                    installed_callbacks = true;
                }
            }
            HtmlScript::Inline(code) if code.trim().is_empty() => {}
            HtmlScript::Inline(code) => {
                assert!(
                    installed_callbacks,
                    "inline WPT script ran before testharness.js"
                );
                runtime
                    .execute_script(format!("wpt:///{relative}#inline-{index}"), code)
                    .unwrap_or_else(|error| panic!("failed to execute {relative}: {error}"));
            }
        }
    }
    assert!(
        installed_callbacks,
        "selected WPT file did not load testharness.js"
    );

    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .unwrap_or_else(|error| panic!("WPT event loop failed for {relative}: {error}"));
    runtime
        .execute_script(
            "deno_audio_wpt_assertions.js",
            format!(
                r#"
                  (() => {{
                    if (!__denoAudioWptDone) {{
                      throw new Error("WPT harness did not complete: {relative}");
                    }}
                    if (__denoAudioWptHarnessStatus.status !== 0) {{
                      throw new Error(`WPT harness failure: ${{JSON.stringify(__denoAudioWptHarnessStatus)}}`);
                    }}
                    if (__denoAudioWptResults.length === 0) {{
                      throw new Error("WPT file produced no tests: {relative}");
                    }}
                    const failures = __denoAudioWptResults.filter((result) => result.status !== 0);
                    if (failures.length !== 0) {{
                      throw new Error(`WPT failures: ${{JSON.stringify(failures)}}`);
                    }}
                  }})();
                "#
            ),
        )
        .unwrap_or_else(|error| panic!("WPT result mismatch for {relative}: {error}"));
}

#[tokio::test(flavor = "current_thread")]
async fn pinned_wpt_subset_matches_reviewed_expectations() {
    let (commit, expectations) = parse_expectations();
    assert_eq!(commit, WPT_COMMIT, "Rust and JSON WPT pins drifted");

    let expected_categories = HashSet::from([
        "constructors",
        "AudioBuffer",
        "connect/disconnect",
        "offline rendering",
    ]);
    let actual_categories: HashSet<_> = expectations
        .iter()
        .map(|entry| entry.category.as_str())
        .collect();
    assert_eq!(
        actual_categories, expected_categories,
        "WPT category coverage drifted"
    );

    let mut paths = HashSet::new();
    for expectation in &expectations {
        assert!(
            paths.insert(&expectation.path),
            "duplicate WPT expectation path"
        );
        match expectation.status.as_str() {
            "pass" => assert!(
                expectation.reason.is_none(),
                "passing WPT cannot have a skip reason"
            ),
            "skip" => assert!(
                expectation
                    .reason
                    .as_ref()
                    .is_some_and(|reason| !reason.trim().is_empty()),
                "every skipped WPT must have a reason"
            ),
            other => panic!("unsupported WPT expectation status: {other}"),
        }
    }

    let Some(root) = configured_wpt_root() else {
        eprintln!("WPT checkout absent; see tests/wpt/README.md. CI supplies commit {WPT_COMMIT}.");
        return;
    };
    verify_wpt_commit(&root, WPT_COMMIT);

    let mut passed = 0;
    let mut skipped = 0;
    for expectation in &expectations {
        if expectation.status == "pass" {
            run_wpt_file(&root, &expectation.path).await;
            passed += 1;
        } else {
            assert!(
                root.join(&expectation.path).is_file(),
                "skipped WPT path is stale"
            );
            skipped += 1;
        }
    }
    println!("WPT Phase 1: {passed} passed, {skipped} skipped with reviewed reasons");
}

#[test]
fn fetch_shim_confines_and_serves_the_pinned_checkout() {
    let Some(root) = configured_wpt_root() else {
        return;
    };
    let bytes = read_wpt_resource(&root, "/resources/testharness.js?cache=off#fragment")
        .expect("fetch shim must serve a pinned WPT resource");
    assert!(bytes.starts_with(b"/*global self*/"));
    assert!(read_wpt_resource(&root, "/%2e%2e/Cargo.toml").is_err());
    assert!(read_wpt_resource(&root, "../Cargo.toml").is_err());
    assert!(read_wpt_resource(&root, "https://example.com/resources/testharness.js").is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn javascript_fetch_shim_exposes_text_and_bytes() {
    let Some(root) = configured_wpt_root() else {
        return;
    };
    let mut runtime = wpt_runtime(&root);
    runtime
        .execute_script("deno_audio_wpt_harness.js", HARNESS_ADAPTER)
        .expect("WPT adapter must install");
    runtime
        .execute_script(
            "deno_audio_wpt_fetch_probe.js",
            r#"
              globalThis.META_TITLE = "webaudio/probe.html";
              if (typeof gc !== "function") {
                throw new Error("WPT runtime does not expose test GC");
              }
              globalThis.__wptFetchProbe = fetch(
                "../resources/testharness.js?cache=off",
              ).then(async (response) => {
                if (!response.ok || response.status !== 200) {
                  throw new Error("fetch response metadata mismatch");
                }
                const text = await response.text();
                const buffer = await response.arrayBuffer();
                if (response.url !== "/resources/testharness.js?cache=off" ||
                    !text.startsWith("/*global self*/") || buffer.byteLength < 1000) {
                  throw new Error("fetch response body mismatch");
                }
                globalThis.__wptFetchProbeDone = true;
              });
            "#,
        )
        .expect("fetch probe must start");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("fetch probe event loop must complete");
    runtime
        .execute_script(
            "deno_audio_wpt_fetch_assert.js",
            r#"
              if (!globalThis.__wptFetchProbeDone) {
                throw new Error("fetch probe did not finish");
              }
            "#,
        )
        .expect("fetch shim must expose text and bytes");
}

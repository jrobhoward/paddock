//! Extensions for native-process apps (ROADMAP Z8h): the fixture
//! executable's `ext` command calls a test extension through the node,
//! with the grant and scope a component's import gets.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::any::Any;
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::services::{
    Extension, ExtensionApp, ExtensionSpec, NativeCall, NativeCallError, NativeReply, StoreData,
    extension_ctx, reply,
};
use bulkhead_host::wasmtime::component::Linker;
use bulkhead_host::{
    ExitReason, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime,
    RuntimeConfig,
};
use common::{ask, native_fixtures, text};
use rstest::rstest;
use serde_json::json;

/// `test:probe`: says what it knows of the caller, repeats a word, and
/// crashes on request.
struct Probe;

/// An instance's state for [`Probe`].
struct Seen {
    granted: bool,
    scope: serde_json::Value,
    app: String,
}

impl Extension for Probe {
    fn package(&self) -> &str {
        "test:probe"
    }

    fn link(&self, _linker: &mut Linker<StoreData>) -> bulkhead_host::wasmtime::Result<()> {
        Ok(())
    }

    fn instance(&self, app: &ExtensionApp<'_>) -> Result<Box<dyn Any + Send>, String> {
        Ok(Box::new(Seen {
            granted: app.granted,
            scope: app.scope.clone(),
            app: app.app.to_string(),
        }))
    }

    fn call<'a>(&'a self, data: &'a mut StoreData, call: NativeCall) -> NativeReply<'a> {
        Box::pin(async move {
            let mut ctx = extension_ctx::<Seen>(data);
            let seen = ctx.state();
            match call.function() {
                "probe#seen" => reply(&(seen.granted, &seen.scope, &seen.app)),
                "probe#repeat" => {
                    let (word, times): (String, u8) = call.args()?;
                    let result: Result<String, String> = if seen.granted {
                        Ok(word.repeat(usize::from(times)))
                    } else {
                        Err("access-denied".into())
                    };
                    reply(&result)
                }
                "probe#crash" => Err(NativeCallError::Crash("asked to".into())),
                _ => Err(call.no_such_function()),
            }
        })
    }
}

/// `test:silent`: serves no function to native apps.
struct Silent;

impl Extension for Silent {
    fn package(&self) -> &str {
        "test:silent"
    }

    fn link(&self, _linker: &mut Linker<StoreData>) -> bulkhead_host::wasmtime::Result<()> {
        Ok(())
    }

    fn instance(&self, _app: &ExtensionApp<'_>) -> Result<Box<dyn Any + Send>, String> {
        Ok(Box::new(()))
    }

    fn call<'a>(&'a self, _data: &'a mut StoreData, call: NativeCall) -> NativeReply<'a> {
        Box::pin(async move { Err(call.no_such_function()) })
    }
}

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let mut native = NativeConfig::default();
        native.runner = Some(native_fixtures().0.clone());
        let extensions: Vec<Arc<dyn Extension>> = vec![Arc::new(Probe), Arc::new(Silent)];
        let rt = Runtime::with_extensions(
            RuntimeConfig {
                max_instances: 16,
                native,
                ..RuntimeConfig::default()
            },
            extensions,
        )
        .unwrap();
        let app = rt.load_native(NativeCode {
            path: native_fixtures().1.clone(),
            kind: NativeProgram::Executable,
        });
        (rt, app)
    })
}

/// How the manifest and the operator treat `test:probe`.
#[derive(Debug, Clone, Copy)]
enum Probing {
    Granted,
    NotGranted,
    Undeclared,
}

fn up(app: &str, probing: Probing) -> InstanceHandle {
    let mut spec = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new(app, "1.0.0")
    };
    if !matches!(probing, Probing::Undeclared) {
        spec.services.extensions.insert(
            "test:probe".into(),
            ExtensionSpec::new(json!({ "word": "hi" })),
        );
    }
    let granted = match probing {
        Probing::Granted => BTreeSet::from(["test:probe".to_string()]),
        _ => BTreeSet::new(),
    };
    let (rt, loaded) = shared();
    rt.spawn(loaded, spec.with_granted_extensions(&granted))
        .unwrap()
}

#[rstest]
#[case::granted(Probing::Granted, r#"[true,{"word":"hi"},"granted"]"#)]
#[case::not_granted(Probing::NotGranted, r#"[false,{"word":"hi"},"not_granted"]"#)]
#[case::undeclared(Probing::Undeclared, r#"[false,null,"undeclared"]"#)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ext____grant_and_scope____as_a_component_gets(
    #[case] probing: Probing,
    #[case] expected: &str,
) {
    let name = format!("{probing:?}")
        .chars()
        .fold(String::new(), |mut s, c| {
            if c.is_uppercase() && !s.is_empty() {
                s.push('_');
            }
            s.push(c.to_ascii_lowercase());
            s
        });
    let instance = up(&name, probing);

    let seen = ask(&instance, "ext test:probe probe#seen null").await;

    assert_eq!(seen, expected);
}

#[rstest]
#[case::result(Probing::Granted, r#"["ab",3]"#, r#"{"Ok":"ababab"}"#)]
#[case::extensions_own_error(Probing::NotGranted, r#"["ab",3]"#, r#"{"Err":"access-denied"}"#)]
#[case::bad_arguments(Probing::Granted, r#"["ab",-1]"#, "err:BadArguments")]
#[case::not_a_tuple(Probing::Granted, r#""ab""#, "err:BadArguments")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ext____arguments_and_result____in_cbor(
    #[case] probing: Probing,
    #[case] args: &str,
    #[case] expected: &str,
) {
    let instance = up("repeat", probing);

    let result = ask(&instance, &format!("ext test:probe probe#repeat {args}")).await;

    assert!(result.starts_with(expected), "{result}");
}

#[rstest]
#[case::no_extension("test:absent probe#seen", "err:NoSuchExtension")]
#[case::no_function("test:probe probe#nope", r#"err:NoSuchFunction("probe#nope")"#)]
#[case::serves_none("test:silent quiet#x", r#"err:NoSuchFunction("quiet#x")"#)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ext____nothing_to_call____app_gets_an_error_and_lives_on(
    #[case] target: &str,
    #[case] expected: &str,
) {
    let instance = up("missing", Probing::Granted);

    let result = ask(&instance, &format!("ext {target} null")).await;
    let after = ask(&instance, "echo alive").await;

    assert_eq!(result, expected);
    assert_eq!(after, "alive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ext____extension_crashes____instance_crashed_as_by_a_trap() {
    let instance = up("crashing", Probing::Granted);

    let _ = instance
        .call(
            text("ext test:probe probe#crash null"),
            Duration::from_secs(5),
        )
        .await;
    let reason = instance.wait().await;

    let ExitReason::Crashed(why) = &reason else {
        panic!("expected a crash, got {reason:?}");
    };
    assert!(why.contains("test:probe: asked to"), "{why}");
}

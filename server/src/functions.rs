//! Typed calls between functions.
//!
//! An app may import a WIT interface that another app exports, say
//! `acme:billing/api`. The host satisfies that import with stubs: calling one
//! instantiates the exporting app and runs its function with the same
//! arguments, copied into the callee's memory. No HTTP or JSON, and the call
//! is type-checked against the interface. Resources can't cross apps (they
//! belong to an instance), so linked interfaces pass plain data.

use crate::app::RequestState;
use crate::platform;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Result, bail, format_err};

/// Interfaces the host itself provides; any other import is a link.
fn provided_by_host(name: &str) -> bool {
    name.starts_with("wasi:") || name.starts_with("platform:")
}

/// The result of matching imports to exports across all apps.
pub struct LinkPlan {
    /// Per app (same order as given): interface → app providing it.
    pub links: Vec<HashMap<String, String>>,
    /// Every linked interface with its function names, for the stubs.
    interfaces: HashMap<String, BTreeSet<String>>,
}

/// Matches each app's non-host imports to the app exporting that interface.
/// `explicit` holds `--link APP:INTERFACE=TARGET` choices; otherwise the
/// interface must be exported by exactly one other app.
pub fn plan(engine: &Engine, apps: &[(String, &Component)], explicit: &[(String, String, String)]) -> Result<LinkPlan> {
    let mut exporters: HashMap<String, Vec<String>> = HashMap::new();
    for (name, component) in apps {
        for (export, _) in component.component_type().exports(engine) {
            if !provided_by_host(export) {
                exporters.entry(export.to_string()).or_default().push(name.clone());
            }
        }
    }
    let mut links = Vec::new();
    let mut interfaces: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (name, component) in apps {
        let mut app_links = HashMap::new();
        let component_type = component.component_type();
        for (import, item) in component_type.imports(engine) {
            if provided_by_host(import) {
                continue;
            }
            let ComponentItem::ComponentInstance(instance) = &item.ty else {
                bail!("app {name:?} imports {import}, which is not an interface");
            };
            let target = match explicit.iter().find(|(app, iface, _)| app == name && iface == import) {
                Some((_, _, target)) => target.as_str(),
                None => match exporters.get(import).map(Vec::as_slice).unwrap_or_default() {
                    [] => bail!("app {name:?} imports {import}, but no app exports it"),
                    [target] => target.as_str(),
                    several => bail!("app {name:?} imports {import}, exported by {several:?}; choose with --link {name}:{import}=APP"),
                },
            };
            if target == name {
                bail!("app {name:?} imports {import} from itself");
            }
            if !exporters.get(import).is_some_and(|e| e.iter().any(|t| t == target)) {
                bail!("--link {name}:{import}={target}: {target:?} doesn't export {import}");
            }
            let functions = interfaces.entry(import.to_string()).or_default();
            for (export, ty) in instance.exports(engine) {
                match ty.ty {
                    ComponentItem::ComponentFunc(_) => {
                        functions.insert(export.to_string());
                    }
                    ComponentItem::Resource(_) => {
                        bail!("app {name:?} imports {import}, which has resource {export:?}; linked interfaces can only pass data")
                    }
                    _ => {}
                }
            }
            eprintln!("link: {name} → {target} ({import})");
            app_links.insert(import.to_string(), target.to_string());
        }
        links.push(app_links);
    }
    Ok(LinkPlan { links, interfaces })
}

/// Defines a stub for every function of every linked interface. A stub
/// finds the caller's target app at call time, so one linker serves apps
/// linked to different providers of the same interface.
pub fn define_stubs(linker: &mut Linker<RequestState>, plan: &LinkPlan) -> Result<()> {
    for (interface, functions) in &plan.interfaces {
        let mut instance = linker.instance(interface)?;
        for function in functions {
            let (interface, function) = (Arc::<str>::from(interface.as_str()), Arc::<str>::from(function.as_str()));
            instance.func_new_async(&function.clone(), move |mut store, _ty, params, results| {
                let (interface, function) = (interface.clone(), function.clone());
                Box::new(async move {
                    let state = store.data();
                    let env = state.env.clone();
                    let target_name = state.links.get(&*interface).cloned();
                    let current = env.platform.current();
                    let target = target_name
                        .as_deref()
                        .and_then(|t| current.routes.app(t))
                        .cloned()
                        .ok_or_else(|| format_err!("no app linked for {interface}"))?;
                    let started = Instant::now();
                    let result = platform::call(&env, &target, &interface, &function, params, results).await;
                    // The callee's time is not the caller's CPU.
                    store.data_mut().call_wait += started.elapsed();
                    result
                })
            })?;
        }
    }
    Ok(())
}
